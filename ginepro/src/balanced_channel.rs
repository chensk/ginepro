//! Provides the builder and implementation of [`GrpcService`] that enables
//! periodic service discovery.

use crate::{
    service_probe::{GrpcServiceProbe, GrpcServiceProbeConfig},
    DnsResolver, LookupService, ServiceDefinition,
};
use anyhow::Context as _;
use http::Request;
use std::{
    convert::TryInto,
    task::{Context, Poll}, sync::atomic::AtomicUsize,
};
use tokio::time::Duration;
use tonic::client::GrpcService;
use tonic::transport::channel::Channel;
use tonic::{body::BoxBody, transport::ClientTlsConfig};
use tower::Service;

// Determines the channel size of the channel we use
// to report endpoint changes to tonic.
// This is effectively how many changes we can report in one go.
// We set the number high to avoid any blocking on our side.
static GRPC_REPORT_ENDPOINTS_CHANNEL_SIZE: usize = 1024;

/// Implements tonic [`GrpcService`] for a client-side load balanced [`Channel`] (using `The Power of
/// Two Choices`).
///
/// [`GrpcService`](tonic::client::GrpcService)
///
/// ```rust
/// #[tokio::main]
/// async fn main() {
///     use ginepro::LoadBalancedChannel;
///     use shared_proto::pb::tester_client::TesterClient;
///     use std::convert::TryInto;
///
///     let load_balanced_channel = LoadBalancedChannel::builder(("my.hostname", 5000))
///         .channel()
///         .await
///         .expect("failed to construct LoadBalancedChannel");
///
///     let client = TesterClient::new(load_balanced_channel);
/// }
/// ```
///
#[derive(Debug)]
pub struct LoadBalancedChannel(Vec<Channel>, AtomicUsize);

impl From<LoadBalancedChannel> for Channel {
    fn from(channel: LoadBalancedChannel) -> Self {
        channel.0.into_iter().take(1).next().unwrap()
    }
}

impl LoadBalancedChannel {
    /// Start configuring a `LoadBalancedChannel` by passing in the [`ServiceDefinition`]
    /// for the gRPC server service you want to call -  e.g. `my.service.uri` and `5000`.
    ///
    /// All the service endpoints of a [`ServiceDefinition`] will be
    /// constructed by resolving IPs for [`ServiceDefinition::hostname`], and
    /// using the port number [`ServiceDefinition::port`].
    pub fn builder<S>(service_definition: S) -> LoadBalancedChannelBuilder<DnsResolver, S>
    where
        S: TryInto<ServiceDefinition> + Send + Sync + 'static,
        S::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + Sync,
    {
        LoadBalancedChannelBuilder::new_with_service(service_definition)
    }

    pub fn get_channel(&self) -> Channel{
        // we don't really need exactly strict ordering, relaxed is okay
        let idx = self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.0.len();
        self.0.get(idx).expect("invalid index").clone()
    }
}

impl Service<http::Request<BoxBody>> for LoadBalancedChannel {
    type Response = http::Response<<Channel as GrpcService<BoxBody>>::ResponseBody>;
    type Error = <Channel as GrpcService<BoxBody>>::Error;
    type Future = <Channel as GrpcService<BoxBody>>::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let idx = self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.0.len();
        GrpcService::poll_ready(self.0.get_mut(idx).expect("invalid index"), cx)
    }

    fn call(&mut self, request: Request<BoxBody>) -> Self::Future {
        let idx = self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.0.len();
        GrpcService::call(self.0.get_mut(idx).expect("invalid index"), request)
    }
}

/// Enumerates the different domain name resolution strategies that
/// the [`LoadBalancedChannelBuilder`] supports.
pub enum ResolutionStrategy {
    /// Creates the channel without attempting to resolve
    /// a set of initial IPs.
    Lazy,
    /// Tries to resolve the domain name before creating the channel
    /// in order to start with a non-empty set of IPs.
    Eager { timeout: Duration },
}

/// Builder to configure and create a [`LoadBalancedChannel`].
pub struct LoadBalancedChannelBuilder<T, S> {
    service_definition: S,
    probe_interval: Option<Duration>,
    resolution_strategy: ResolutionStrategy,
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    keep_alive_timeout: Option<Duration>,
    http2_keep_alive_interval: Option<Duration>,
    keep_alive_while_idle: bool,
    tls_config: Option<ClientTlsConfig>,
    lookup_service: Option<T>,
    connection_number: usize,
}

impl<S> LoadBalancedChannelBuilder<DnsResolver, S>
where
    S: TryInto<ServiceDefinition> + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + Sync,
{
    /// Set the [`ServiceDefinition`] of the gRPC server service
    /// -  e.g. `my.service.uri` and `5000`.
    ///
    /// All the service endpoints of a [`ServiceDefinition`] will be
    /// constructed by resolving all ips from [`ServiceDefinition::hostname`], and
    /// using the portnumber [`ServiceDefinition::port`].
    pub fn new_with_service(service_definition: S) -> LoadBalancedChannelBuilder<DnsResolver, S> {
        Self {
            service_definition,
            probe_interval: None,
            timeout: None,
            connect_timeout: None,
            tls_config: None,
            lookup_service: None,
            resolution_strategy: ResolutionStrategy::Lazy,
            keep_alive_timeout: None,
            http2_keep_alive_interval: None,
            keep_alive_while_idle: false,
            connection_number: 1,
        }
    }

    /// Set a custom [`LookupService`].
    pub fn lookup_service<T: LookupService + Send + Sync + 'static>(
        self,
        lookup_service: T,
    ) -> LoadBalancedChannelBuilder<T, S> {
        LoadBalancedChannelBuilder {
            lookup_service: Some(lookup_service),
            service_definition: self.service_definition,
            probe_interval: self.probe_interval,
            tls_config: self.tls_config,
            timeout: self.timeout,
            connect_timeout: self.connect_timeout,
            resolution_strategy: self.resolution_strategy,
            keep_alive_timeout: self.keep_alive_timeout,
            http2_keep_alive_interval: self.http2_keep_alive_interval,
            keep_alive_while_idle: self.keep_alive_while_idle,
            connection_number: self.connection_number,
        }
    }
}

impl<T: LookupService + Send + Sync + 'static + Sized + Clone, S> LoadBalancedChannelBuilder<T, S>
where
    S: TryInto<ServiceDefinition> + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + Sync,
{
    /// Set the how often, the client should probe for changes to  gRPC server endpoints.
    /// Default interval in seconds is 10.
    pub fn dns_probe_interval(self, interval: Duration) -> LoadBalancedChannelBuilder<T, S> {
        Self {
            probe_interval: Some(interval),
            ..self
        }
    }

    /// Set a request timeout that will be applied to every new `Endpoint`.
    pub fn timeout(self, timeout: Duration) -> LoadBalancedChannelBuilder<T, S> {
        Self {
            timeout: Some(timeout),
            ..self
        }
    }

    /// Set a connection timeout that will be applied to every new `Endpoint`.
    ///
    /// Defaults to the overall request `timeout` if not set.
    pub fn connect_timeout(self, connection_timeout: Duration) -> LoadBalancedChannelBuilder<T, S> {
        Self {
            connect_timeout: Some(connection_timeout),
            ..self
        }
    }

    pub fn keep_alive_timeout(self, keep_alive_timeout: Duration) -> LoadBalancedChannelBuilder<T, S> {
        Self {
            keep_alive_timeout: Some(keep_alive_timeout),
            ..self
        }
    }

    pub fn http2_keep_alive_interval(self, http2_keep_alive_interval: Duration) -> LoadBalancedChannelBuilder<T, S> {
        Self {
            http2_keep_alive_interval: Some(http2_keep_alive_interval),
            ..self
        }
    }

    pub fn keep_alive_while_idle(self, keep_alive_while_idle: bool) -> LoadBalancedChannelBuilder<T, S> {
        Self {
            keep_alive_while_idle: keep_alive_while_idle,
            ..self
        }
    }

    pub fn connection_number(self, connection_number: usize) -> LoadBalancedChannelBuilder<T, S>{
        // 100 connections at most to avoid resource exhausted
        let connection_number = if connection_number > 100{
            100
        }else{
            connection_number
        };
        Self{
            connection_number,
            ..self
        }
    }

    /// Set the [`ResolutionStrategy`].
    ///
    /// Default set to [`ResolutionStrategy::Lazy`].
    ///
    /// If [`ResolutionStrategy::Lazy`] the domain name will be resolved after-the-fact.
    ///
    /// Instead, if [`ResolutionStrategy::Eager`] is set the domain name will be attempted resolved
    /// once before the [`LoadBalancedChannel`] is created, which ensures that the channel
    /// will have a non-empty of IPs on startup. If it fails the channel creation will also fail.
    pub fn resolution_strategy(
        self,
        resolution_strategy: ResolutionStrategy,
    ) -> LoadBalancedChannelBuilder<T, S> {
        Self {
            resolution_strategy,
            ..self
        }
    }

    /// Configure the channel to use tls.
    /// A `tls_config` MUST be specified to use the `HTTPS` scheme.
    pub fn with_tls(self, tls_config: ClientTlsConfig) -> LoadBalancedChannelBuilder<T, S> {
        Self {
            tls_config: Some(tls_config),
            ..self
        }
    }

    /// Construct a [`LoadBalancedChannel`] from the [`LoadBalancedChannelBuilder`] instance.
    pub async fn channel(mut self) -> Result<LoadBalancedChannel, anyhow::Error> {
        match self.lookup_service.take() {
            Some(lookup_service) => self.channel_inner(lookup_service).await,
            None => {
                self.channel_inner(DnsResolver::from_system_config().await?)
                    .await
            }
        }
    }

    async fn channel_inner<U: LookupService>(
        self,
        lookup_service: U,
    ) -> Result<LoadBalancedChannel, anyhow::Error>
    where
        U: LookupService + Send + Sync + 'static + Sized + Clone,
    {
        let service_definition = self.service_definition.try_into().map_err(Into::into).map_err(|err| anyhow::anyhow!(err))?;
        let dns_lookup = lookup_service.clone();
        let mut channels = Vec::with_capacity(self.connection_number);
        for _ in 0..self.connection_number{
            let (channel, sender) = Channel::balance_channel(GRPC_REPORT_ENDPOINTS_CHANNEL_SIZE);
    
            let config = GrpcServiceProbeConfig {
                service_definition: service_definition.clone(),
                dns_lookup: dns_lookup.clone(),
                endpoint_timeout: self.timeout,
                endpoint_connect_timeout: self.connect_timeout.or(self.timeout),
                
                probe_interval: self
                    .probe_interval
                    .unwrap_or_else(|| Duration::from_secs(10)),
                keep_alive_timeout: self.keep_alive_timeout,
                http2_keep_alive_interval: self.http2_keep_alive_interval,
                keep_alive_while_idle: self.keep_alive_while_idle,
            };
    
            let tls_config = self.tls_config.clone().map(|mut tls_config| {
                // Since we resolve the hostname to an IP, which is not a valid DNS name,
                // we have to set the hostname explicitly on the tls config,
                // otherwise the IP will be set as the domain name and tls handshake will fail.
                tls_config = tls_config.domain_name(config.service_definition.hostname());
    
                tls_config
            });
    
            let mut service_probe = GrpcServiceProbe::new_with_reporter(config, sender);
    
            if let Some(tls_config) = tls_config {
                service_probe = service_probe.with_tls(tls_config);
            }
    
            if let ResolutionStrategy::Eager { timeout } = self.resolution_strategy {
                // Make sure we resolve the hostname once before we create the channel.
                tokio::time::timeout(timeout, service_probe.probe_once())
                    .await
                    .context("timeout out while attempting to resolve IPs")?
                    .context("failed to resolve IPs")?;
            }
    
            tokio::spawn(service_probe.probe());
            channels.push(channel);
        }

        Ok(LoadBalancedChannel(channels, AtomicUsize::new(0)))
    }
}

const _: () = {
    const fn assert_is_send<T: Send>() {}
    assert_is_send::<LoadBalancedChannelBuilder<DnsResolver, ServiceDefinition>>();
    assert_is_send::<LoadBalancedChannel>();
};
