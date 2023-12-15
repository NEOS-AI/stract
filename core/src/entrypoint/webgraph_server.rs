// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::net::SocketAddr;
use std::sync::Arc;

use itertools::Itertools;
use serde::Deserialize;
use serde::Serialize;
use tracing::info;
use url::Url;
use utoipa::ToSchema;

use crate::config;
use crate::distributed::cluster::Cluster;
use crate::distributed::member::Member;
use crate::distributed::member::Service;
use crate::distributed::sonic;
use crate::distributed::sonic::service::Message;
use crate::ranking::inbound_similarity::InboundSimilarity;
use crate::searcher::DistributedSearcher;
use crate::similar_hosts::SimilarHostsFinder;
use crate::sonic_service;
use crate::webgraph::Compression;
use crate::webgraph::FullEdge;
use crate::webgraph::Node;
use crate::webgraph::Webgraph;
use crate::webgraph::WebgraphBuilder;
use crate::Result;

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScoredHost {
    pub host: String,
    pub score: f64,
    pub description: Option<String>,
}

const MAX_HOSTS: usize = 20;

pub struct WebGraphService {
    searcher: DistributedSearcher,
    similar_hosts_finder: SimilarHostsFinder,
    host_graph: Arc<Webgraph>,
    page_graph: Arc<Webgraph>,
}

sonic_service!(
    WebGraphService,
    [SimilarHosts, Knows, IngoingLinks, OutgoingLinks]
);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarHosts {
    pub hosts: Vec<String>,
    pub top_n: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Knows {
    pub host: String,
}

#[async_trait::async_trait]
impl Message<WebGraphService> for SimilarHosts {
    type Response = Vec<ScoredHost>;

    async fn handle(self, server: &WebGraphService) -> sonic::Result<Self::Response> {
        let sites = &self.hosts[..std::cmp::min(self.hosts.len(), MAX_HOSTS)];
        let similar_hosts = server
            .similar_hosts_finder
            .find_similar_hosts(sites, self.top_n);

        let urls = similar_hosts
            .iter()
            .filter_map(|s| Url::parse(&("http://".to_string() + s.node.name.as_str())).ok())
            .collect_vec();

        let descriptions = server.searcher.get_homepage_descriptions(&urls).await;

        let similar_hosts = similar_hosts
            .into_iter()
            .map(|site| {
                let description = Url::parse(&("http://".to_string() + site.node.name.as_str()))
                    .ok()
                    .and_then(|url| descriptions.get(&url).cloned());

                ScoredHost {
                    host: site.node.name,
                    score: site.score,
                    description,
                }
            })
            .collect_vec();

        Ok(similar_hosts)
    }
}

#[async_trait::async_trait]
impl Message<WebGraphService> for Knows {
    type Response = Option<Node>;

    async fn handle(self, server: &WebGraphService) -> sonic::Result<Self::Response> {
        let url = Url::parse(&("http://".to_string() + self.host.as_str()))
            .map_err(|_| sonic::Error::BadRequest)?;

        let node = Node::from(url).into_host();

        if server.similar_hosts_finder.knows_about(&node) {
            Ok(Some(node))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GraphLevel {
    Host,
    Page,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngoingLinks {
    pub node: Node,
    pub level: GraphLevel,
}

#[async_trait::async_trait]
impl Message<WebGraphService> for IngoingLinks {
    type Response = Vec<FullEdge>;

    async fn handle(self, server: &WebGraphService) -> sonic::Result<Self::Response> {
        match self.level {
            GraphLevel::Host => {
                let node = self.node.into_host();
                Ok(server.host_graph.ingoing_edges(node))
            }
            GraphLevel::Page => Ok(server.page_graph.ingoing_edges(self.node)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingLinks {
    pub node: Node,
    pub level: GraphLevel,
}

#[async_trait::async_trait]
impl Message<WebGraphService> for OutgoingLinks {
    type Response = Vec<FullEdge>;

    async fn handle(self, server: &WebGraphService) -> sonic::Result<Self::Response> {
        match self.level {
            GraphLevel::Host => {
                let node = self.node.into_host();
                Ok(server.host_graph.outgoing_edges(node))
            }
            GraphLevel::Page => Ok(server.page_graph.outgoing_edges(self.node)),
        }
    }
}

pub async fn run(config: config::WebgraphServerConfig) -> Result<()> {
    let addr: SocketAddr = config.host;

    // dropping the handle leaves the cluster
    let cluster = Arc::new(
        Cluster::join(
            Member {
                id: config.cluster_id,
                service: Service::Webgraph { host: addr },
            },
            config.gossip_addr,
            config.gossip_seed_nodes.unwrap_or_default(),
        )
        .await?,
    );
    let searcher = DistributedSearcher::new(cluster);

    let host_graph = Arc::new(
        WebgraphBuilder::new(config.host_graph_path)
            .compression(Compression::Lz4)
            .open(),
    );
    let page_graph = Arc::new(
        WebgraphBuilder::new(config.page_graph_path)
            .compression(Compression::Lz4)
            .open(),
    );
    let inbound_similarity = InboundSimilarity::open(config.inbound_similarity_path)?;

    let similar_hosts_finder = SimilarHostsFinder::new(
        Arc::clone(&host_graph),
        inbound_similarity,
        config.max_similar_hosts,
    );

    let server = WebGraphService {
        host_graph,
        page_graph,
        searcher,
        similar_hosts_finder,
    }
    .bind(addr)
    .await
    .unwrap();

    info!("webgraph server is ready to accept requests on {}", addr);

    loop {
        if let Err(e) = server.accept().await {
            tracing::error!("{:?}", e);
        }
    }
}
