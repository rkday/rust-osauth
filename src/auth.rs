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

//! Base code for authentication.

use std::fmt::Debug;

use futures::{future, Future};
use reqwest::r#async::{Client, RequestBuilder};
use reqwest::{IntoUrl, Method, Url};

use super::Error;

/// Trait for an authentication type.
///
/// An OpenStack authentication type is expected to be able to:
///
/// 1. get an authentication token to use when accessing services,
/// 2. get an endpoint URL for the given service type.
///
/// An authentication type should cache the token as long as it's valid.
pub trait AuthType: Debug + Sync + Send {
    /// Get a URL for the requested service.
    fn get_endpoint(
        &self,
        service_type: String,
        endpoint_interface: Option<String>,
    ) -> Box<Future<Item = Url, Error = Error> + Send>;

    /// Create an authenticated request.
    fn request(
        &self,
        method: Method,
        url: Url,
    ) -> Box<Future<Item = RequestBuilder, Error = Error> + Send>;

    /// Refresh the authentication (renew the token, etc).
    fn refresh(&self) -> Box<Future<Item = (), Error = Error> + Send>;

    /// Region used with this authentication (if any).
    fn region(&self) -> Option<String> {
        None
    }
}

/// Authentication type that provides no authentication.
///
/// This type always uses a pre-defined endpoint and sends no authenticaiton information:
/// ```rust,no_run
/// let auth = osauth::NoAuth::new("https://cloud.local/baremetal")
///     .expect("Invalid auth URL");
/// let session = osauth::Session::new(auth);
/// ```
#[derive(Clone, Debug)]
pub struct NoAuth {
    client: Client,
    endpoint: Url,
}

impl NoAuth {
    /// Create a new fake authentication method using a fixed endpoint.
    ///
    /// This endpoint will be returned in response to all `get_endpoint` calls
    /// of the [AuthType](trait.AuthType.html) trait.
    pub fn new<U>(endpoint: U) -> Result<NoAuth, Error>
    where
        U: IntoUrl,
    {
        Ok(NoAuth {
            client: Client::new(),
            endpoint: endpoint.into_url()?,
        })
    }
}

impl AuthType for NoAuth {
    /// Create a request.
    fn request(
        &self,
        method: Method,
        url: Url,
    ) -> Box<Future<Item = RequestBuilder, Error = Error> + Send> {
        Box::new(future::ok(self.client.request(method, url)))
    }

    /// Get a predefined endpoint for all service types
    fn get_endpoint(
        &self,
        _service_type: String,
        _endpoint_interface: Option<String>,
    ) -> Box<Future<Item = Url, Error = Error> + Send> {
        Box::new(future::ok(self.endpoint.clone()))
    }

    /// This call does nothing for `NoAuth`.
    fn refresh(&self) -> Box<Future<Item = (), Error = Error> + Send> {
        Box::new(future::ok(()))
    }
}

#[cfg(test)]
pub mod test {
    use futures::Future;

    use super::{AuthType, NoAuth};

    #[test]
    fn test_noauth_new() {
        let a = NoAuth::new("http://127.0.0.1:8080/v1").unwrap();
        let e = a.endpoint;
        assert_eq!(e.scheme(), "http");
        assert_eq!(e.host_str().unwrap(), "127.0.0.1");
        assert_eq!(e.port().unwrap(), 8080u16);
        assert_eq!(e.path(), "/v1");
    }

    #[test]
    fn test_noauth_new_fail() {
        let _ = NoAuth::new("foo bar").err().unwrap();
    }

    #[test]
    fn test_noauth_get_endpoint() {
        let a = NoAuth::new("http://127.0.0.1:8080/v1").unwrap();
        let e = a.get_endpoint(String::from("foobar"), None).wait().unwrap();
        assert_eq!(e.scheme(), "http");
        assert_eq!(e.host_str().unwrap(), "127.0.0.1");
        assert_eq!(e.port().unwrap(), 8080u16);
        assert_eq!(e.path(), "/v1");
    }
}
