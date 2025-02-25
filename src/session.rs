// Copyright 2019 Dmitry Tantsur <divius.inside@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Session structure definition.

use std::sync::Arc;

use futures::future;
use futures::prelude::*;
use log::{debug, trace};
use reqwest::header::HeaderMap;
use reqwest::r#async::{RequestBuilder, Response};
use reqwest::{Method, Url};
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::cache;
use super::protocol::ServiceInfo;
use super::request;
use super::services::ServiceType;
use super::url;
use super::{Adapter, ApiVersion, AuthType, Error};

type Cache = cache::MapCache<&'static str, ServiceInfo>;

/// An OpenStack API session.
///
/// The session object serves as a wrapper around an [authentication type](trait.AuthType.html),
/// providing convenient methods to make HTTP requests and work with microversions.
///
/// # Note
///
/// All clones of one session share the same authentication and endpoint cache. Use
/// [with_auth_type](#method.with_auth_type) to detach a session.
#[derive(Debug, Clone)]
pub struct Session {
    auth: Arc<AuthType>,
    cached_info: Arc<Cache>,
    endpoint_interface: Option<String>,
}

impl Session {
    /// Create a new session with a given authentication plugin.
    ///
    /// The resulting session will use the default endpoint interface (usually,
    /// public).
    pub fn new<Auth: AuthType + 'static>(auth_type: Auth) -> Session {
        Session {
            auth: Arc::new(auth_type),
            cached_info: Arc::new(cache::MapCache::default()),
            endpoint_interface: None,
        }
    }

    /// Create an adapter for the specific service type.
    ///
    /// The new `Adapter` will share the same authentication and will initially use the same
    /// endpoint interface (although it can be changed later without affecting the `Session`).
    ///
    /// If you don't need the `Session` any more, using [into_adapter](#method.into_adapter) is a
    /// bit more efficient.
    ///
    /// ```rust,no_run
    /// let session =
    ///     osauth::from_env().expect("Failed to create an identity provider from the environment");
    /// let adapter = session.adapter(osauth::services::COMPUTE);
    /// ```
    #[inline]
    pub fn adapter<Srv>(&self, service: Srv) -> Adapter<Srv> {
        Adapter::from_session(self.clone(), service)
    }

    /// Create an adapter for the specific service type.
    ///
    /// The new `Adapter` will share the same authentication and will initially use the same
    /// endpoint interface (although it can be changed later without affecting the `Session`).
    ///
    /// This method is a bit more efficient than [adapter](#method.adapter) since it does not
    /// involve cloning internal structures.
    ///
    /// ```rust,no_run
    /// let session =
    ///     osauth::from_env().expect("Failed to create an identity provider from the environment");
    /// let adapter = session.into_adapter(osauth::services::COMPUTE);
    /// ```
    #[inline]
    pub fn into_adapter<Srv>(self, service: Srv) -> Adapter<Srv> {
        Adapter::from_session(self, service)
    }

    /// Get a reference to the authentication type in use.
    #[inline]
    pub fn auth_type(&self) -> &AuthType {
        self.auth.as_ref()
    }

    /// Endpoint interface in use (if any).
    #[inline]
    pub fn endpoint_interface(&self) -> &Option<String> {
        &self.endpoint_interface
    }

    /// Update the authentication and purges cached endpoint information.
    ///
    /// # Warning
    ///
    /// Authentication will also be updated for clones of this `Session`, since they share the same
    /// authentication object.
    #[inline]
    pub fn refresh(&mut self) -> impl Future<Item = (), Error = Error> + Send {
        self.reset_cache();
        self.auth.refresh()
    }

    /// Reset the internal cache.
    #[inline]
    fn reset_cache(&mut self) {
        self.cached_info = Arc::new(cache::MapCache::default());
    }

    /// Set a new authentication for this `Session`.
    ///
    /// This call clears the cached service information for this `Session`.
    /// It does not, however, affect clones of this `Session`.
    #[inline]
    pub fn set_auth_type<Auth: AuthType + 'static>(&mut self, auth_type: Auth) {
        self.reset_cache();
        self.auth = Arc::new(auth_type);
    }

    /// Set endpoint interface to use.
    ///
    /// This call clears the cached service information for this `Session`.
    /// It does not, however, affect clones of this `Session`.
    pub fn set_endpoint_interface<S>(&mut self, endpoint_interface: S)
    where
        S: Into<String>,
    {
        self.reset_cache();
        self.endpoint_interface = Some(endpoint_interface.into());
    }

    /// Convert this session into one using the given authentication.
    #[inline]
    pub fn with_auth_type<Auth: AuthType + 'static>(mut self, auth_method: Auth) -> Session {
        self.set_auth_type(auth_method);
        self
    }

    /// Convert this session into one using the given endpoint interface.
    #[inline]
    pub fn with_endpoint_interface<S>(mut self, endpoint_interface: S) -> Session
    where
        S: Into<String>,
    {
        self.set_endpoint_interface(endpoint_interface);
        self
    }

    /// Get minimum/maximum API (micro)version information.
    ///
    /// Returns `None` if the range cannot be determined, which usually means
    /// that microversioning is not supported.
    ///
    /// ```rust,no_run
    /// use futures::Future;
    ///
    /// let session =
    ///     osauth::from_env().expect("Failed to create an identity provider from the environment");
    /// let future = session
    ///     .get_api_versions(osauth::services::COMPUTE)
    ///     .map(|maybe_versions| {
    ///         if let Some((min, max)) = maybe_versions {
    ///             println!("The compute service supports versions {} to {}", min, max);
    ///         } else {
    ///             println!("The compute service does not support microversioning");
    ///         }
    ///     });
    /// ```
    pub fn get_api_versions<Srv: ServiceType + Send>(
        &self,
        service: Srv,
    ) -> impl Future<Item = Option<(ApiVersion, ApiVersion)>, Error = Error> + Send {
        self.extract_service_info(service, |info| {
            match (info.minimum_version, info.current_version) {
                (Some(min), Some(max)) => Some((min, max)),
                _ => None,
            }
        })
    }

    /// Construct and endpoint for the given service from the path.
    ///
    /// You won't need to use this call most of the time, since all request calls can fetch the
    /// endpoint automatically.
    pub fn get_endpoint<Srv, I>(
        &self,
        service: Srv,
        path: I,
    ) -> impl Future<Item = Url, Error = Error> + Send
    where
        Srv: ServiceType + Send,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
    {
        let path_iter = path.into_iter();
        self.extract_service_info(service, |info| {
            url::extend(info.root_url.clone(), path_iter)
        })
    }

    /// Get the currently used major version from the given service.
    ///
    /// Can return `None` if the service does not support API version discovery at all.
    pub fn get_major_version<Srv: ServiceType + Send>(
        &self,
        service: Srv,
    ) -> impl Future<Item = Option<ApiVersion>, Error = Error> + Send {
        self.extract_service_info(service, |info| info.major_version)
    }

    /// Pick the highest API version supported by the service.
    ///
    /// Returns `None` if none of the requested versions are available.
    ///
    /// ```rust,no_run
    /// use futures::Future;
    ///
    /// let session =
    ///     osauth::from_env().expect("Failed to create an identity provider from the environment");
    /// let candidates = vec![osauth::ApiVersion(1, 2), osauth::ApiVersion(1, 42)];
    /// let future = session
    ///     .pick_api_version(osauth::services::COMPUTE, candidates)
    ///     .and_then(|maybe_version| {
    ///         if let Some(version) = maybe_version {
    ///             println!("Using version {}", version);
    ///         } else {
    ///             println!("Using the base version");
    ///         }
    ///         session.get(osauth::services::COMPUTE, &["servers"], maybe_version)
    ///     });
    /// ```
    pub fn pick_api_version<Srv, I>(
        &self,
        service: Srv,
        versions: I,
    ) -> impl Future<Item = Option<ApiVersion>, Error = Error> + Send
    where
        Srv: ServiceType + Send,
        I: IntoIterator<Item = ApiVersion>,
        I::IntoIter: Send,
    {
        let vers = versions.into_iter();
        if vers.size_hint().1 == Some(0) {
            future::Either::A(future::ok(None))
        } else {
            future::Either::B(self.extract_service_info(service, |info| {
                vers.filter(|item| info.supports_api_version(*item)).max()
            }))
        }
    }

    /// Check if the service supports the API version.
    pub fn supports_api_version<Srv: ServiceType + Send>(
        &self,
        service: Srv,
        version: ApiVersion,
    ) -> impl Future<Item = bool, Error = Error> + Send {
        self.pick_api_version(service, Some(version))
            .map(|x| x.is_some())
    }

    /// Make an HTTP request to the given service.
    ///
    /// The `service` argument is an object implementing the
    /// [ServiceType](services/trait.ServiceType.html) trait. Some known service types are available
    /// in the [services](services/index.html) module.
    ///
    /// The `path` argument is a URL path without the service endpoint (e.g. `/servers/1234`).
    ///
    /// If `api_version` is set, it is send with the request to enable a higher API version.
    /// Otherwise the base API version is used. You can use
    /// [pick_api_version](#method.pick_api_version) to choose an API version to use.
    ///
    /// The result is a `RequestBuilder` that can be customized further. Error checking and response
    /// parsing can be done using functions from the [request](request/index.html) module.
    ///
    /// ```rust,no_run
    /// use futures::Future;
    /// use reqwest::Method;
    ///
    /// let session =
    ///     osauth::from_env().expect("Failed to create an identity provider from the environment");
    /// let future = session
    ///     .request(osauth::services::COMPUTE, Method::HEAD, &["servers", "1234"], None)
    ///     .then(osauth::request::send_checked)
    ///     .map(|response| {
    ///         println!("Response: {:?}", response);
    ///     });
    /// ```
    ///
    /// This is the most generic call to make a request. You may prefer to use more specific `get`,
    /// `post`, `put` or `delete` calls instead.
    pub fn request<Srv, I>(
        &self,
        service: Srv,
        method: Method,
        path: I,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = RequestBuilder, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
    {
        let auth = Arc::clone(&self.auth);
        self.get_endpoint(service.clone(), path)
            .and_then(move |url| {
                trace!(
                    "Sending HTTP {} request to {} with API version {:?}",
                    method,
                    url,
                    api_version
                );
                auth.request(method, url)
            })
            .and_then(move |mut builder| {
                if let Some(version) = api_version {
                    let mut headers = HeaderMap::new();
                    match service.set_api_version_headers(&mut headers, version) {
                        Ok(()) => builder = builder.headers(headers),
                        Err(err) => return future::err(err),
                    }
                }
                future::ok(builder)
            })
    }

    /// Start a GET request.
    ///
    /// Use this call if you need some advanced features of the resulting `RequestBuilder`.
    /// Otherwise use:
    /// * [get](#method.get) to issue a generic GET without a query.
    /// * [get_query](#method.get_query) to issue a generic GET with a query.
    /// * [get_json](#method.get_json) to issue GET and parse a JSON result.
    /// * [get_json_query](#method.get_json_query) to issue GET with a query and parse a JSON
    ///   result.
    ///
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    #[deprecated(since = "0.2.3", note = "Use request")]
    pub fn start_get<Srv, I>(
        &self,
        service: Srv,
        path: I,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = RequestBuilder, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
    {
        self.request(service, Method::GET, path, api_version)
    }

    /// Issue a GET request.
    ///
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    pub fn get<Srv, I>(
        &self,
        service: Srv,
        path: I,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = Response, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
    {
        self.request(service, Method::GET, path, api_version)
            .then(request::send_checked)
    }

    /// Fetch a JSON using the GET request.
    ///
    /// ```rust,no_run
    /// use futures::Future;
    /// use osproto::common::IdAndName;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize)]
    /// pub struct ServersRoot {
    ///     pub servers: Vec<IdAndName>,
    /// }
    ///
    /// let session =
    ///     osauth::from_env().expect("Failed to create an identity provider from the environment");
    ///
    /// let future = session
    ///     .get_json(osauth::services::COMPUTE, &["servers"], None)
    ///     .map(|servers: ServersRoot| {
    ///         for srv in servers.servers {
    ///             println!("ID = {}, Name = {}", srv.id, srv.name);
    ///         }
    ///     });
    /// ```
    ///
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    pub fn get_json<Srv, I, T>(
        &self,
        service: Srv,
        path: I,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = T, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
        T: DeserializeOwned + Send,
    {
        self.request(service, Method::GET, path, api_version)
            .then(request::fetch_json)
    }

    /// Fetch a JSON using the GET request with a query.
    ///
    /// See `reqwest` crate documentation for how to define a query.
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    pub fn get_json_query<Srv, I, Q, T>(
        &self,
        service: Srv,
        path: I,
        query: Q,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = T, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
        Q: Serialize + Send,
        T: DeserializeOwned + Send,
    {
        self.request(service, Method::GET, path, api_version)
            .map(move |builder| builder.query(&query))
            .then(request::fetch_json)
    }

    /// Issue a GET request with a query
    ///
    /// See `reqwest` crate documentation for how to define a query.
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    pub fn get_query<Srv, I, Q>(
        &self,
        service: Srv,
        path: I,
        query: Q,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = Response, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
        Q: Serialize + Send,
    {
        self.request(service, Method::GET, path, api_version)
            .map(move |builder| builder.query(&query))
            .then(request::send_checked)
    }

    /// Start a POST request.
    ///
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    #[deprecated(since = "0.2.3", note = "Use request")]
    pub fn start_post<Srv, I>(
        &self,
        service: Srv,
        path: I,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = RequestBuilder, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
    {
        self.request(service, Method::POST, path, api_version)
    }

    /// POST a JSON object.
    ///
    /// The `body` argument is anything that can be serialized into JSON.
    ///
    /// See [request](#method.request) for an explanation of the other parameters.
    #[inline]
    pub fn post<Srv, I, T>(
        &self,
        service: Srv,
        path: I,
        body: T,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = Response, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
        T: Serialize + Send,
    {
        self.request(service, Method::POST, path, api_version)
            .map(move |builder| builder.json(&body))
            .then(request::send_checked)
    }

    /// POST a JSON object and receive a JSON back.
    ///
    /// The `body` argument is anything that can be serialized into JSON.
    ///
    /// See [request](#method.request) for an explanation of the other parameters.
    #[inline]
    pub fn post_json<Srv, I, T, R>(
        &self,
        service: Srv,
        path: I,
        body: T,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = R, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
        T: Serialize + Send,
        R: DeserializeOwned + Send,
    {
        self.request(service, Method::POST, path, api_version)
            .map(move |builder| builder.json(&body))
            .then(request::fetch_json)
    }

    /// Start a PUT request.
    ///
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    #[deprecated(since = "0.2.3", note = "Use request")]
    pub fn start_put<Srv, I>(
        &self,
        service: Srv,
        path: I,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = RequestBuilder, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
    {
        self.request(service, Method::PUT, path, api_version)
    }

    /// PUT a JSON object.
    ///
    /// The `body` argument is anything that can be serialized into JSON.
    ///
    /// See [request](#method.request) for an explanation of the other parameters.
    #[inline]
    pub fn put<Srv, I, T>(
        &self,
        service: Srv,
        path: I,
        body: T,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = Response, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
        T: Serialize + Send,
    {
        self.request(service, Method::PUT, path, api_version)
            .map(move |builder| builder.json(&body))
            .then(request::send_checked)
    }

    /// Issue an empty PUT request.
    ///
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    pub fn put_empty<Srv, I>(
        &self,
        service: Srv,
        path: I,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = Response, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
    {
        self.request(service, Method::PUT, path, api_version)
            .then(request::send_checked)
    }

    /// PUT a JSON object and receive a JSON back.
    ///
    /// The `body` argument is anything that can be serialized into JSON.
    ///
    /// See [request](#method.request) for an explanation of the other parameters.
    #[inline]
    pub fn put_json<Srv, I, T, R>(
        &self,
        service: Srv,
        path: I,
        body: T,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = R, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
        T: Serialize + Send,
        R: DeserializeOwned + Send,
    {
        self.request(service, Method::PUT, path, api_version)
            .map(move |builder| builder.json(&body))
            .then(request::fetch_json)
    }

    /// Start a DELETE request.
    ///
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    #[deprecated(since = "0.2.3", note = "Use request")]
    pub fn start_delete<Srv, I>(
        &self,
        service: Srv,
        path: I,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = RequestBuilder, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
    {
        self.request(service, Method::DELETE, path, api_version)
    }

    /// Issue a DELETE request.
    ///
    /// See [request](#method.request) for an explanation of the parameters.
    #[inline]
    pub fn delete<Srv, I>(
        &self,
        service: Srv,
        path: I,
        api_version: Option<ApiVersion>,
    ) -> impl Future<Item = Response, Error = Error> + Send
    where
        Srv: ServiceType + Send + Clone,
        I: IntoIterator,
        I::Item: AsRef<str>,
        I::IntoIter: Send,
    {
        self.request(service, Method::DELETE, path, api_version)
            .then(request::send_checked)
    }

    /// Ensure service info and return the cache.
    fn extract_service_info<Srv, F, T>(
        &self,
        service: Srv,
        filter: F,
    ) -> impl Future<Item = T, Error = Error>
    where
        Srv: ServiceType + Send,
        F: FnOnce(&ServiceInfo) -> T + Send,
        T: Send,
    {
        let catalog_type = service.catalog_type();
        if self.cached_info.is_set(&catalog_type) {
            future::Either::A(future::ok(
                self.cached_info
                    .extract(&catalog_type, filter)
                    .expect("BUG: cached record removed while in extract_service_info"),
            ))
        } else {
            debug!(
                "No cached information for service {}, fetching",
                catalog_type
            );

            let endpoint_interface = self.endpoint_interface.clone();
            let cached_info = Arc::clone(&self.cached_info);
            let auth_type = Arc::clone(&self.auth);
            future::Either::B(
                self.auth
                    .get_endpoint(catalog_type.to_string(), endpoint_interface)
                    .and_then(move |ep| ServiceInfo::fetch(service, ep, auth_type))
                    .map(move |info| {
                        let value = filter(&info);
                        cached_info.set(catalog_type, info);
                        value
                    }),
            )
        }
    }

    #[cfg(test)]
    pub(crate) fn cache_fake_service(
        &mut self,
        service_type: &'static str,
        service_info: ServiceInfo,
    ) {
        let _ = self.cached_info.set(service_type, service_info);
    }
}

#[cfg(test)]
pub(crate) mod test {
    use futures::Future;
    use reqwest::Url;

    use super::super::protocol::ServiceInfo;
    use super::super::services::{GenericService, VersionSelector};
    use super::super::{ApiVersion, NoAuth};
    use super::Session;

    pub const URL: &str = "http://127.0.0.1:5000/";

    pub const URL_WITH_SUFFIX: &str = "http://127.0.0.1:5000/v2/servers";

    pub fn new_simple_session(url: &str) -> Session {
        let service_info = ServiceInfo {
            root_url: Url::parse(url).unwrap(),
            major_version: None,
            minimum_version: None,
            current_version: None,
        };
        new_session(url, service_info)
    }

    pub fn new_session(url: &str, service_info: ServiceInfo) -> Session {
        let auth = NoAuth::new(url).unwrap();
        let mut session = Session::new(auth);
        session.cache_fake_service("fake", service_info);
        session
    }

    pub const FAKE: GenericService = GenericService::new("fake", VersionSelector::Any);

    #[test]
    fn test_get_endpoint() {
        let s = new_simple_session(URL);
        let ep = s.get_endpoint(FAKE, &[""]).wait().unwrap();
        assert_eq!(&ep.to_string(), URL);
    }

    #[test]
    fn test_get_endpoint_slice() {
        let s = new_simple_session(URL);
        let ep = s.get_endpoint(FAKE, &["v2", "servers"]).wait().unwrap();
        assert_eq!(&ep.to_string(), URL_WITH_SUFFIX);
    }

    #[test]
    fn test_get_endpoint_vec() {
        let s = new_simple_session(URL);
        let ep = s
            .get_endpoint(FAKE, vec!["v2".to_string(), "servers".to_string()])
            .wait()
            .unwrap();
        assert_eq!(&ep.to_string(), URL_WITH_SUFFIX);
    }

    #[test]
    fn test_get_major_version_absent() {
        let s = new_simple_session(URL);
        let res = s.get_major_version(FAKE).wait().unwrap();
        assert!(res.is_none());
    }

    pub const MAJOR_VERSION: ApiVersion = ApiVersion(2, 0);

    #[test]
    fn test_get_major_version_present() {
        let service_info = ServiceInfo {
            root_url: Url::parse(URL).unwrap(),
            major_version: Some(MAJOR_VERSION),
            minimum_version: None,
            current_version: None,
        };
        let s = new_session(URL, service_info);
        let res = s.get_major_version(FAKE).wait().unwrap();
        assert_eq!(res, Some(MAJOR_VERSION));
    }

    pub const MIN_VERSION: ApiVersion = ApiVersion(2, 1);
    pub const MAX_VERSION: ApiVersion = ApiVersion(2, 42);

    pub fn fake_service_info() -> ServiceInfo {
        ServiceInfo {
            root_url: Url::parse(URL).unwrap(),
            major_version: Some(MAJOR_VERSION),
            minimum_version: Some(MIN_VERSION),
            current_version: Some(MAX_VERSION),
        }
    }

    #[test]
    fn test_pick_api_version_empty() {
        let service_info = fake_service_info();
        let s = new_session(URL, service_info);
        let res = s.pick_api_version(FAKE, None).wait().unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn test_pick_api_version_empty_vec() {
        let service_info = fake_service_info();
        let s = new_session(URL, service_info);
        let res = s.pick_api_version(FAKE, Vec::new()).wait().unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn test_pick_api_version() {
        let service_info = fake_service_info();
        let s = new_session(URL, service_info);
        let choice = vec![
            ApiVersion(2, 0),
            ApiVersion(2, 2),
            ApiVersion(2, 4),
            ApiVersion(2, 99),
        ];
        let res = s.pick_api_version(FAKE, choice).wait().unwrap();
        assert_eq!(res, Some(ApiVersion(2, 4)));
    }

    #[test]
    fn test_pick_api_version_option() {
        let service_info = fake_service_info();
        let s = new_session(URL, service_info);
        let res = s
            .pick_api_version(FAKE, Some(ApiVersion(2, 4)))
            .wait()
            .unwrap();
        assert_eq!(res, Some(ApiVersion(2, 4)));
    }

    #[test]
    fn test_pick_api_version_impossible() {
        let service_info = fake_service_info();
        let s = new_session(URL, service_info);
        let choice = vec![ApiVersion(2, 0), ApiVersion(2, 99)];
        let res = s.pick_api_version(FAKE, choice).wait().unwrap();
        assert!(res.is_none());
    }
}
