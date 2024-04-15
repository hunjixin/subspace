use super::farm::{DiskFarm, DsnArgs};
use crate::utils::shutdown_signal;
use anyhow::{anyhow, Result};
use clap::{Parser, ValueHint};
use futures::channel::mpsc::channel;
use futures::channel::oneshot;
use futures::{select, FutureExt, SinkExt, StreamExt};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::{Arc, RwLock};
use subspace_core_primitives::crypto::blake3_hash_list;
use subspace_core_primitives::crypto::kzg::{embedded_kzg_settings, Kzg};
use subspace_core_primitives::{Piece, PieceIndex, SegmentIndex};
use subspace_farmer::node_client::NodeClientExt;
use subspace_farmer::utils::piece_validator::SegmentCommitmentPieceValidator;
use subspace_farmer::utils::run_future_in_dedicated_thread;
use subspace_farmer::{Identity, NodeClient, NodeRetryRpcClient, KNOWN_PEERS_CACHE_SIZE};
use subspace_networking::libp2p::identity::{ed25519, Keypair};
use subspace_networking::libp2p::kad::{ProviderRecord, RecordKey};
use subspace_networking::libp2p::multiaddr::Protocol;
use subspace_networking::utils::piece_provider::PieceProvider;
use subspace_networking::utils::strip_peer_id;
use subspace_networking::{
    construct, Config, KademliaMode, KnownPeersManager, KnownPeersManagerConfig,
    LocalRecordProvider, Node, NodeRunner, PieceByIndexRequest, PieceByIndexRequestHandler,
    PieceByIndexResponse, SegmentHeaderBySegmentIndexesRequestHandler, SegmentHeaderRequest,
    SegmentHeaderResponse,
};

use subspace_rpc_primitives::MAX_SEGMENT_HEADERS_PER_REQUEST;
use tokio::sync::Semaphore;
use tokio::time::{sleep, Duration, Instant};
use tracing::{debug, error, info, warn, Instrument};
use zeroize::Zeroizing;

/// Arguments for farmer
#[derive(Debug, Parser)]
pub struct CacheServerArgs {
    cache_path: DiskFarm,
    /// WebSocket RPC URL of the Subspace node to connect to
    #[arg(long, value_hint = ValueHint::Url, default_value = "ws://127.0.0.1:9944")]
    node_rpc_url: String,

    #[arg(long, default_value_t = 10)]
    download_count: u32,

    #[arg(long, default_value_t = false)]
    pub verify_piece: bool,

    #[arg(long, default_value_t = false)]
    pub disable_detect_future_piece: bool,

    /// DSN parameters
    #[clap(flatten)]
    dsn: DsnArgs,
}

/// Start farming by using multiple replica plot in specified path and connecting to WebSocket
/// server at specified address.
pub async fn cache_server(cache_server_args: CacheServerArgs) -> anyhow::Result<()> {
    let signal = shutdown_signal();

    let CacheServerArgs {
        node_rpc_url,
        download_count,
        disable_detect_future_piece,
        verify_piece,
        mut dsn,
        cache_path,
    } = cache_server_args;

    if !cache_path.directory.exists() {
        if let Err(error) = fs::create_dir(&cache_path.directory) {
            return Err(anyhow!(
                "Directory {} doesn't exist and can't be created: {}",
                cache_path.directory.display(),
                error
            ));
        }
    }

    let piece_dir = cache_path.directory.join("piece");
    if !piece_dir.exists() {
        if let Err(error) = fs::create_dir(piece_dir.clone()) {
            return Err(anyhow!(
                "Piece Dir {} doesn't exist and can't be created: {}",
                cache_path.directory.display(),
                error
            ));
        }
    }
    let mut piece_storage = MyPieceCache::new(piece_dir);
    info!("Start to load Pieces Cache");
    piece_storage.load_piece(verify_piece)?;
    info!(
        "Loaded Pieces Cache {}",
        piece_storage.piece_count().unwrap_or(0)
    );

    info!(url = %node_rpc_url, "Connecting to node RPC");
    let node_client = NodeRetryRpcClient::new(&node_rpc_url).await?;

    let farmer_app_info = node_client
        .farmer_app_info()
        .await
        .map_err(|error| anyhow::anyhow!(error))?;

    let identity = Identity::open_or_create(cache_path.directory.clone())
        .map_err(|error| anyhow!("Failed to open or create identity: {error}"))?;
    let keypair = derive_libp2p_keypair(identity.secret_key());

    let (node, mut node_runner) = {
        if dsn.bootstrap_nodes.is_empty() {
            dsn.bootstrap_nodes = farmer_app_info.dsn_bootstrap_nodes.clone();
        }

        configure_dsn(
            hex::encode(farmer_app_info.genesis_hash),
            cache_path.directory.as_path(),
            keypair,
            dsn,
            node_client.clone(),
            piece_storage.clone(),
        )?
    };

    node.listeners().into_iter().for_each(|lst| {
        info!("DSN listening on {}", lst.to_string());
    });

    let networking_fut = run_future_in_dedicated_thread(
        move || async move { node_runner.run().await },
        "farmer-networking".to_string(),
    )?;

    let (sender, mut reciever) = channel::<(PieceIndex, bool,oneshot::Sender<Option<()>>)>(50);
    {
        let node = node.clone();
        let node_client = node_client.clone();
        let piece_storage = piece_storage.clone();
        //download piece
        tokio::spawn(async move {
            let kzg = Kzg::new(embedded_kzg_settings());
            let validator = Some(SegmentCommitmentPieceValidator::new(
                node.clone(),
                node_client.clone(),
                kzg.clone(),
            ));

            let piece_storage = Arc::new(RwLock::new(piece_storage));
            let semaphore = Arc::new(Semaphore::new(download_count as usize));
            info!("Start piece download consumer");
            loop {
                if let Some((piece_index,only_l1, result_sender)) = reciever.next().await {
                    if piece_storage.read().unwrap().has_piece(&piece_index) {
                        return;
                    }

                    let node = node.clone();
                    let validator = validator.clone();
                    let piece_storage = piece_storage.clone();
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    let _ = tokio::spawn(async move {
                        let piece_provider = PieceProvider::new(node, validator);

                        info!(%piece_index, "Start to download piece from cache");
                        let start = Instant::now();
                        if let Some(piece) = piece_provider.get_piece_from_cache(piece_index).await
                        {
                            piece_storage
                                .write()
                                .unwrap()
                                .save_piece(piece_index.clone(), piece)
                                .expect("Write piece file to storage");
                            let duration = start.elapsed();
                            drop(permit);
                            info!(%piece_index, "Downloaded piece from L1 {:?}", duration);
                            result_sender.send(Some(())).expect("Send result");
                            return;
                        }

                        if !only_l1 {
                            info!(%piece_index, "Start to download piece archival storage");
                            let start = Instant::now();
                            if let Some(piece) = piece_provider
                                .get_piece_from_archival_storage(piece_index, 15)
                                .await
                            {
                                piece_storage
                                    .write()
                                    .unwrap()
                                    .save_piece(piece_index, piece)
                                    .expect("Write piece file to storage");
                                let duration = start.elapsed();
                                drop(permit);
                                info!(%piece_index, "Downloaded piece from archival storage {:?}", duration);
                                result_sender.send(Some(())).expect("Send result");
                                return;
                            }
                        }
       
                        error!(%piece_index, "Unable to download piece wait for next round");
                        result_sender.send(None).expect("Send result");
                        drop(permit);
                    });
                }
            }
        });
    }

    //subscribe new piece
    {
        let node_client = node_client.clone();
        let mut sender = sender.clone();
        let duration = Duration::from_secs(5);
        tokio::spawn(async move {
            loop {
                let segment_headers_notifications =
                    node_client.subscribe_archived_segment_headers().await;
                match segment_headers_notifications {
                    Ok(mut segment_headers_notifications) => {
                        info!("Begin to subscribe segment header notification");
                        loop {
                            select! {
                                maybe_segment_header = segment_headers_notifications.next().fuse() => {
                                    if let Some(segment_header) = maybe_segment_header {
                                        let segment_index = segment_header.segment_index();
                                        info!(%segment_index, "Starting to process newly archived segment");
                                        let piecse_indexs = segment_index.segment_piece_indexes();
                                        for piece_index in piecse_indexs {
                                            let (result_sender, _) = oneshot::channel::<Option<()>>();
                                            if let Err(e) = sender.send((piece_index, false, result_sender)).await {
                                                warn!(%e, "Send piece index fail");
                                                continue;
                                            }
                                        }
                                    } else {
                                        // Keep-up sync only ends with subscription, which lasts for duration of an
                                        // instance
                                        warn!(" Keep-up sync only ends with subscription");
                                        sleep(duration).await;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(%e, "Subscribe archived segment headers");
                        sleep(duration).await;
                    }
                }
            }
        })
    };

    {
        //scan to fix missing piece and try fix missing round by round
        let node_client = node_client.clone();
        let mut sender = sender.clone();
        let duration = Duration::from_secs(60 * 2);
        tokio::spawn(async move {
            info!("Start to fix missing pieces");
            loop {
                let farmer_app_info = node_client.farmer_app_info().await;
                match farmer_app_info {
                    Ok(farmer_app_info) => {
                        let last_segment_index =
                            farmer_app_info.protocol_info.history_size.segment_index();
                        let missing_pieces: Vec<PieceIndex> = (SegmentIndex::ZERO
                            ..=last_segment_index)
                            .map(|segment_index| segment_index.segment_piece_indexes())
                            .flatten()
                            .filter(|a| !piece_storage.has_piece(a))
                            .rev()
                            .collect();
                        if missing_pieces.len() > 0 {
                            info!("Start to download missing pieces {}", missing_pieces.len());
                            for piece_index in missing_pieces {
                                let (result_sender, _) = oneshot::channel::<Option<()>>();
                                if let Err(e) = sender.send((piece_index, false, result_sender)).await {
                                    warn!(%e, "Send piece index fail");
                                    continue;
                                }
                            }
                        }

                        if !disable_detect_future_piece {
                            info!("Try to increase piece index");
                            if let Ok(max_piece_index) = piece_storage.max_piece_index() {
                                let mut next_piece_index = max_piece_index + PieceIndex::ONE;
                                loop {
                                    let (result_sender, result_recevier) = oneshot::channel::<Option<()>>();
                                    if let Err(e) = sender.send((next_piece_index, true, result_sender)).await {
                                        warn!(%e, "Send piece index fail");
                                        continue;
                                    }
                                    
                                    //wait result to stop detect
                                    match  result_recevier.await {
                                        Ok(result) => {
                                            match result  {
                                                Some(()) => info!(%next_piece_index, "Success get piece more and node maybe sync slow"),
                                                None=>{
                                                    info!("no new piece index dectect");
                                                    break;
                                                }
                                            }
                                        }
                                        Err(e) =>{
                                            error!(%e, "receive result fail");
                                            break;
                                        }
                                    }
                                    next_piece_index += PieceIndex::ONE;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(%e, "Reuqust for latest message fail");
                    }
                }
                sleep(duration).await;
            }
        });
    };

    let networking_fut = networking_fut;
    let networking_fut = pin!(networking_fut);
    futures::select!(
        // Signal future
        _ = signal.fuse() => {},

        // Networking future
        _ = networking_fut.fuse() => {
            info!("Node runner exited.")
        },

    );

    anyhow::Ok(())
}

fn derive_libp2p_keypair(schnorrkel_sk: &schnorrkel::SecretKey) -> Keypair {
    let mut secret_bytes = Zeroizing::new(schnorrkel_sk.to_ed25519_bytes());

    let keypair = ed25519::Keypair::from(
        ed25519::SecretKey::try_from_bytes(&mut secret_bytes.as_mut()[..32])
            .expect("Secret key is exactly 32 bytes in size; qed"),
    );

    Keypair::from(keypair)
}

#[derive(Debug, Clone)]
struct MyPieceCache {
    dir: PathBuf,
    pieces: Arc<RwLock<HashSet<PieceIndex>>>,
}

impl MyPieceCache {
    fn new(dir: PathBuf) -> Self {
        MyPieceCache {
            dir: dir,
            pieces: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    fn load_piece(&mut self, verify: bool) -> Result<()> {
        let pieces = self.pieces.write().map_err(|e| anyhow!(e.to_string()))?;
        let entries = fs::read_dir(&self.dir)?;
        let segment_dirs = entries.filter_map(|a| {
            if a.is_err() {
                return None;
            }
            let entry = a.unwrap();
            let ft = entry.file_type();
            if ft.is_err() {
                return None;
            }
            if !ft.unwrap().is_dir() {
                return None;
            }

            return Some(entry.path());
        });

        let _pieces = segment_dirs
            .map(|segement_dir| {
                let entries = fs::read_dir(segement_dir).expect("read sub directory");
                entries
                    .filter_map(|a| {
                        if a.is_err() {
                            return None;
                        }
                        let entry = a.unwrap();
                        let ft = entry.file_type();
                        if ft.is_err() {
                            return None;
                        }
                        if ft.unwrap().is_dir() {
                            return None;
                        }
                        let file_name =  entry.file_name();
                        let file_name = file_name.to_str();
                        if file_name.is_none() {
                            return None
                        }
                        let file_name = file_name.unwrap();
                        if file_name.contains(".checksum") {
                            return None
                        }
                        let  piece_index = file_name.parse::<u64>() ;
                        if piece_index.is_err() {
                            return  None;
                        }

                        return Some((entry,PieceIndex::from(piece_index.unwrap())));
                    })
                    .filter(|(entry, piece_index)| {
                        if let Ok(metadata) = fs::metadata(entry.path().clone()) {
                            let file_size = metadata.len();
                            if file_size == Piece::SIZE as u64 {
                                if verify {
                                    //todo make checksum become a function
                                    let (_, piece_path, checksum_path) = self.piece_path(&piece_index);
                                    let piece_index_bytes = piece_index.to_bytes();
                                    let expected_checksum = fs::read(checksum_path.clone()).expect("Piece checkksum exit");
                                    let content = fs::read(piece_path.clone()).expect("Piece file exit");
                                    let actual_checksum = blake3_hash_list(&[piece_index_bytes.as_slice(), content.as_ref()]);
                                    if actual_checksum != *expected_checksum {
                                        warn!(
                                            actual_checksum = %hex::encode(actual_checksum),
                                            expected_checksum = %hex::encode(expected_checksum),
                                            "Hash doesn't match, corrupted piece in cache, remove this piece file"
                                        );

                                        let _ = fs::remove_file(piece_path).map_err(|e|error!(%piece_index,%e,"Fail to remove invalid piece file")).map(|_|error!(%piece_index, "Remove invalid piece file"));
                                        let _ = fs::remove_file(checksum_path).map_err(|e|error!(%piece_index,%e,"Fail to remove invalid piece checksum file")).map(|_|error!(%piece_index, "Remove invalid piece checksum file"));
                                        return false;
                                    }
                                }
                                return true;
                            }
                            fs::remove_file(entry.path()).expect("Remove invalid file");
                        }
                        return false;
                    })
            })
            .flatten()
            .fold(pieces, |mut pieces, (_, piece_index)| {
                pieces.insert(PieceIndex::from(piece_index));
                pieces
            });

        Ok(())
    }

    fn has_piece(&self, index: &PieceIndex) -> bool {
        let pieces = self
            .pieces
            .read()
            .map_err(|e| anyhow!(e.to_string()))
            .unwrap();
        pieces.contains(index)
    }

    fn piece_count(&self) -> Result<usize> {
        let pieces = self.pieces.write().map_err(|e| anyhow!(e.to_string()))?;
        Ok(pieces.len())
    }

    fn max_piece_index(&self) -> Result<PieceIndex> {
        let pieces = self.pieces.read().map_err(|e| anyhow!(e.to_string()))?;
        Ok(*pieces.iter().max().unwrap_or(&PieceIndex::ZERO))
    }

    fn get_piece(&self, piece_index: &PieceIndex) -> Result<Piece> {
        let _pieces = self.pieces.write().map_err(|e| anyhow!(e.to_string()))?;
        let (_, piece_path, _) = self.piece_path(&piece_index);
        if !piece_path.as_path().is_file() {
            return Err(anyhow!("piece not file or not exit"));
        }
        let content = fs::read(piece_path.clone()).expect("Piece file exit");
        Ok(Piece::try_from(content)?)
    }

    #[allow(dead_code)]
    fn get_piece_and_check(&mut self, piece_index: &PieceIndex) -> Result<Piece> {
        let mut pieces = self.pieces.write().map_err(|e| anyhow!(e.to_string()))?;
        let (_, piece_path, checksum) = self.piece_path(&piece_index);
        if !piece_path.as_path().is_file() {
            return Err(anyhow!("piece not file or not exit"));
        }

        let piece_index_bytes = piece_index.to_bytes();
        let expected_checksum = fs::read(checksum.clone()).expect("Piece checkksum exit");
        let content = fs::read(piece_path.clone()).expect("Piece file exit");
        let actual_checksum = blake3_hash_list(&[piece_index_bytes.as_slice(), content.as_ref()]);
        if actual_checksum != *expected_checksum {
            warn!(
                actual_checksum = %hex::encode(actual_checksum),
                expected_checksum = %hex::encode(expected_checksum),
                "Hash doesn't match, corrupted piece in cache, remove this piece file"
            );

            fs::remove_file(piece_path).expect("Remove invalid file");
            fs::remove_file(checksum).expect("Remove invalid file");
            pieces.remove(piece_index);
            return Err(anyhow!("piece check doesn't match"));
        }
        Ok(Piece::try_from(content)?)
    }

    fn save_piece(&mut self, piece_index: PieceIndex, piece: Piece) -> Result<()> {
        let mut pieces = self.pieces.write().map_err(|e| anyhow!(e.to_string()))?;
        let (segment_dir, piece_path, checksum) = self.piece_path(&piece_index);

        if !segment_dir.exists() {
            fs::create_dir(&segment_dir)?;
        }

        if !piece_path.exists() {
            let piece_index_bytes = piece_index.to_bytes();
            let hash = blake3_hash_list(&[&piece_index_bytes, piece.as_ref()]);
            fs::write(checksum, hash)?;
            fs::write(piece_path, piece)?;
        }
        pieces.insert(piece_index);
        Ok(())
    }

    fn piece_path(&self, index: &PieceIndex) -> (PathBuf, PathBuf, PathBuf) {
        let segment_key = index.segment_index();
        let piece_key = index.to_string();
        let segment_dir = self.dir.join(segment_key.to_string());
        let piece_path = segment_dir.join(piece_key.clone());
        let check_sum_path = segment_dir.join(piece_key + ".checksum");
        return (segment_dir, piece_path, check_sum_path);
    }
}

impl LocalRecordProvider for MyPieceCache {
    fn record(&self, _key: &RecordKey) -> Option<ProviderRecord> {
        None
    }
}

/// How many segment headers can be requested at a time.
///
/// Must be the same as RPC limit since all requests go to the node anyway.
const SEGMENT_HEADER_NUMBER_LIMIT: u64 = MAX_SEGMENT_HEADERS_PER_REQUEST as u64;

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn configure_dsn(
    protocol_prefix: String,
    base_path: &Path,
    keypair: Keypair,
    DsnArgs {
        listen_on,
        mut bootstrap_nodes,
        allow_private_ips,
        reserved_peers,
        in_connections,
        out_connections,
        pending_in_connections,
        pending_out_connections,
        external_addresses,
        disable_bootstrap_on_start,
    }: DsnArgs,
    node_client: NodeRetryRpcClient,
    piece_storage: MyPieceCache,
) -> Result<(Node, NodeRunner<MyPieceCache>), anyhow::Error> {
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

    let default_config = Config::new(protocol_prefix, keypair, piece_storage.clone(), None);

    let handler =   PieceByIndexRequestHandler::create(move |_, &PieceByIndexRequest { piece_index }| {
        info!(?piece_index, "Piece request received. Trying cache...");
        let piece_storage = piece_storage.clone();
        async move {
            let piece_from_cache = piece_storage.get_piece(&piece_index);
            if let Err(e) = piece_from_cache {
                warn!(%e, %piece_index,"get piece fail");
                return None;
            }
            Some(PieceByIndexResponse {
                piece: Some(piece_from_cache.unwrap()),
            })
        }
        .in_current_span()
    });
    handler.protocol_config().request_timeout = std::time::Duration::from_secs(120);
    bootstrap_nodes.extend(reserved_peers.clone());
    let config = Config {
        reserved_peers,
        listen_on,
        allow_non_global_addresses_in_dht: allow_private_ips,
        networking_parameters_registry,
        request_response_protocols: vec![handler,
            SegmentHeaderBySegmentIndexesRequestHandler::create(move |_, req| {
                info!(?req, "Segment headers request received.");

                let node_client = node_client.clone();
                let req = req.clone();

                async move {
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
