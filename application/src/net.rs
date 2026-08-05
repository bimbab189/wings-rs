use hickory_resolver::{
    TokioResolver,
    config::LookupIpStrategy,
    lookup_ip::{LookupIp, LookupIpIter},
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::{net::SocketAddr, str::FromStr, sync::Arc};

pub fn host_to_ip(host: &str) -> Option<std::net::IpAddr> {
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    std::net::IpAddr::from_str(host).ok()
}

pub fn is_blocked_ip(cidrs: &[cidr::IpCidr], ip: &std::net::IpAddr) -> bool {
    let ip = ip.to_canonical();

    cidrs.iter().any(|cidr| cidr.contains(&ip))
}

#[derive(Clone)]
pub struct BlockedIpResolver {
    config: Arc<crate::config::Config>,
    selector: fn(&crate::config::InnerConfig) -> &Vec<cidr::IpCidr>,
    context: &'static str,
    state: Arc<TokioResolver>,
}

impl BlockedIpResolver {
    pub fn new(
        config: &Arc<crate::config::Config>,
        selector: fn(&crate::config::InnerConfig) -> &Vec<cidr::IpCidr>,
        context: &'static str,
    ) -> Self {
        let mut builder =
            TokioResolver::builder_tokio().expect("failed to create TokioResolver builder");
        builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4AndIpv6;

        Self {
            config: Arc::clone(config),
            selector,
            context,
            state: Arc::new(builder.build().expect("failed to build TokioResolver")),
        }
    }
}

impl Resolve for BlockedIpResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.clone();

        Box::pin(async move {
            let lookup = resolver.state.lookup_ip(name.as_str()).await?;
            let addrs: Addrs = Box::new(SocketAddrs::new(
                Arc::clone(&resolver.config),
                resolver.selector,
                resolver.context,
                lookup,
                |l| l.iter(),
            ));

            Ok(addrs)
        })
    }
}

#[ouroboros::self_referencing]
struct SocketAddrs {
    config: Arc<crate::config::Config>,
    selector: fn(&crate::config::InnerConfig) -> &Vec<cidr::IpCidr>,
    context: &'static str,
    lookup: LookupIp,

    #[borrows(mut lookup)]
    #[covariant]
    iter: LookupIpIter<'this>,
}

impl Iterator for SocketAddrs {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self
            .with_iter_mut(|iter| iter.next())
            .map(|ip_addr| SocketAddr::new(ip_addr, 0))?;

        let config = self.borrow_config().load();
        if is_blocked_ip((self.borrow_selector())(&config), &next.ip()) {
            tracing::warn!(
                "blocking internal IP address in {}: {}",
                self.borrow_context(),
                next.ip()
            );

            return self.next();
        }

        Some(next)
    }
}
