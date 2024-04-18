use crate::commands::farm::DsnArgs;
use parking_lot::Mutex;
use prometheus_client::registry::Registry;
use std::collections::HashSet;
use std::env;
use std::path::Path;
use std::sync::{Arc, Weak};
use subspace_farmer::farmer_cache::FarmerCache;
use subspace_farmer::node_client::NodeClientExt;
use subspace_farmer::utils::plotted_pieces::PlottedPieces;
use subspace_farmer::{NodeClient, NodeRpcClient, KNOWN_PEERS_CACHE_SIZE};
use subspace_networking::libp2p::identity::Keypair;
use subspace_networking::libp2p::kad::RecordKey;
use subspace_networking::libp2p::multiaddr::Protocol;
use subspace_networking::utils::multihash::ToMultihash;
use subspace_networking::utils::strip_peer_id;
use subspace_networking::{
    construct, Config, KademliaMode, KnownPeersManager, KnownPeersManagerConfig, Node, NodeRunner,
    PieceByIndexRequest, PieceByIndexRequestHandler, PieceByIndexResponse,
    SegmentHeaderBySegmentIndexesRequestHandler, SegmentHeaderRequest, SegmentHeaderResponse,
};
use subspace_rpc_primitives::MAX_SEGMENT_HEADERS_PER_REQUEST;
use tracing::{debug, error, info, Instrument};

/// How many segment headers can be requested at a time.
///
/// Must be the same as RPC limit since all requests go to the node anyway.
const SEGMENT_HEADER_NUMBER_LIMIT: u64 = MAX_SEGMENT_HEADERS_PER_REQUEST as u64;

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(super) fn configure_dsn(
    protocol_prefix: String,
    base_path: &Path,
    keypair: Keypair,
    DsnArgs {
        listen_on,
        bootstrap_nodes,
        allow_private_ips,
        reserved_peers,
        in_connections,
        out_connections,
        pending_in_connections,
        pending_out_connections,
        external_addresses,
        disable_bootstrap_on_start,
    }: DsnArgs,
    weak_plotted_pieces: Weak<Mutex<Option<PlottedPieces>>>,
    node_client: NodeRpcClient,
    farmer_cache: FarmerCache,
    prometheus_metrics_registry: Option<&mut Registry>,
) -> Result<(Node, NodeRunner<FarmerCache>), anyhow::Error> {
    let networking_parameters_registry = KnownPeersManager::new(KnownPeersManagerConfig {
        path: Some(base_path.join("known_addresses.bin").into_boxed_path()),
        ignore_peer_list: strip_peer_id(bootstrap_nodes.clone())
            .into_iter()
            .map(|(peer_id, _)| peer_id)
            .collect::<HashSet<_>>(),
        cache_size: KNOWN_PEERS_CACHE_SIZE,
        ..Default::default()
    })
    .map(Box::new)?;

    let default_config = Config::new(
        protocol_prefix,
        keypair,
        farmer_cache.clone(),
        prometheus_metrics_registry,
    );
    let refuse_request = env::var("REFUSE_REQUEST").unwrap_or("".to_string()) == "1";
    let config = Config {
        reserved_peers,
        listen_on,
        allow_non_global_addresses_in_dht: allow_private_ips,
        networking_parameters_registry,
        request_response_protocols: vec![
            PieceByIndexRequestHandler::create(move |_, &PieceByIndexRequest { piece_index }| {
                info!(?piece_index, "Piece request received. Trying cache...");

                let weak_plotted_pieces = weak_plotted_pieces.clone();
                let farmer_cache = farmer_cache.clone();

                async move {
                    if refuse_request {
                        return None;
                    }

                    let key = RecordKey::from(piece_index.to_multihash());
                    let piece_from_cache = farmer_cache.get_piece(key).await;

                    if let Some(piece) = piece_from_cache {
                        Some(PieceByIndexResponse { piece: Some(piece) })
                    } else {
                        debug!(
                            ?piece_index,
                            "No piece in the cache. Trying archival storage..."
                        );

                        let read_piece_fut = {
                            let plotted_pieces = match weak_plotted_pieces.upgrade() {
                                Some(plotted_pieces) => plotted_pieces,
                                None => {
                                    debug!("A readers and pieces are already dropped");
                                    return None;
                                }
                            };
                            let plotted_pieces = plotted_pieces.lock();
                            let plotted_pieces = match plotted_pieces.as_ref() {
                                Some(plotted_pieces) => plotted_pieces,
                                None => {
                                    debug!(
                                        ?piece_index,
                                        "Readers and pieces are not initialized yet"
                                    );
                                    return None;
                                }
                            };

                            plotted_pieces.read_piece(piece_index)?.in_current_span()
                        };

                        let piece = read_piece_fut.await;

                        Some(PieceByIndexResponse { piece })
                    }
                }
                .in_current_span()
            }),
            SegmentHeaderBySegmentIndexesRequestHandler::create(move |_, req| {
                info!(?req, "Segment headers request received.");

                let node_client = node_client.clone();
                let req = req.clone();

                async move {
                    if refuse_request {
                        return None;
                    }
                    
                    let internal_result = match req {
                        SegmentHeaderRequest::SegmentIndexes { segment_indexes } => {
                            debug!(
                                segment_indexes_count = ?segment_indexes.len(),
                                "Segment headers request received."
                            );

                            node_client.segment_headers(segment_indexes).await
                        }
                        SegmentHeaderRequest::LastSegmentHeaders {
                            mut segment_header_number,
                        } => {
                            if segment_header_number > SEGMENT_HEADER_NUMBER_LIMIT {
                                debug!(
                                    %segment_header_number,
                                    "Segment header number exceeded the limit."
                                );

                                segment_header_number = SEGMENT_HEADER_NUMBER_LIMIT;
                            }
                            node_client
                                .last_segment_headers(segment_header_number)
                                .await
                        }
                    };

                    match internal_result {
                        Ok(segment_headers) => segment_headers
                            .into_iter()
                            .map(|maybe_segment_header| {
                                if maybe_segment_header.is_none() {
                                    error!("Received empty optional segment header!");
                                }
                                maybe_segment_header
                            })
                            .collect::<Option<Vec<_>>>()
                            .map(|segment_headers| SegmentHeaderResponse { segment_headers }),
                        Err(error) => {
                            error!(%error, "Failed to get segment headers from cache");

                            None
                        }
                    }
                }
                .in_current_span()
            }),
        ],
        max_established_outgoing_connections: out_connections,
        max_pending_outgoing_connections: pending_out_connections,
        max_established_incoming_connections: in_connections,
        max_pending_incoming_connections: pending_in_connections,
        bootstrap_addresses: bootstrap_nodes,
        kademlia_mode: KademliaMode::Dynamic,
        external_addresses,
        disable_bootstrap_on_start,
        ..default_config
    };

    construct(config)
        .map(|(node, node_runner)| {
            node.on_new_listener(Arc::new({
                let node = node.clone();

                move |address| {
                    info!(
                        "DSN listening on {}",
                        address.clone().with(Protocol::P2p(node.id()))
                    );
                }
            }))
            .detach();

            // Consider returning HandlerId instead of each `detach()` calls for other usages.
            (node, node_runner)
        })
        .map_err(Into::into)
}
