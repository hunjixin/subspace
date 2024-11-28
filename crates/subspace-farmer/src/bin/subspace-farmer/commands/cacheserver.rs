use crate::commands::shared::DiskFarm;
use crate::utils::shutdown_signal;
use anyhow::{anyhow, Result};
use clap::{Parser, ValueHint};
use futures::channel::mpsc::channel;
use futures::channel::oneshot;
use futures::{select, FutureExt, SinkExt, StreamExt};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::{Arc, RwLock};
use subspace_core_primitives::hashes::blake3_hash_list;
use subspace_core_primitives::pieces::{Piece, PieceIndex};
use subspace_core_primitives::segments::SegmentIndex;
use subspace_farmer::farmer_piece_getter::piece_validator::SegmentCommitmentPieceValidator;
use subspace_farmer::node_client::node_retry_rpc_client::NodeRetryRpcClient;
use subspace_farmer::node_client::{NodeClient, NodeClientExt};
use subspace_farmer::single_disk_farm::identity::Identity;
use subspace_farmer::utils::run_future_in_dedicated_thread;
use subspace_kzg::Kzg;
use subspace_networking::libp2p::identity::{ed25519, Keypair};
use subspace_networking::libp2p::kad::{ProviderRecord, RecordKey};
use subspace_networking::libp2p::multiaddr::Protocol;
use subspace_networking::protocols::request_response::handlers::cached_piece_by_index::{
    CachedPieceByIndexRequest, CachedPieceByIndexRequestHandler, CachedPieceByIndexResponse,
    PieceResult,
};
use subspace_networking::protocols::request_response::handlers::piece_by_index::{
    PieceByIndexRequest, PieceByIndexRequestHandler, PieceByIndexResponse,
};
use subspace_networking::protocols::request_response::handlers::segment_header::{
    SegmentHeaderBySegmentIndexesRequestHandler, SegmentHeaderRequest, SegmentHeaderResponse,
};
use subspace_networking::utils::multihash::ToMultihash;
use subspace_networking::utils::piece_provider::PieceProvider;
use subspace_networking::utils::strip_peer_id;
use subspace_networking::LocalRecordProvider;

use subspace_farmer::KNOWN_PEERS_CACHE_SIZE;
use subspace_networking::{
    construct, Config, KademliaMode, KnownPeersManager, KnownPeersManagerConfig, Node, NodeRunner,
    WeakNode,
};
use subspace_rpc_primitives::MAX_SEGMENT_HEADERS_PER_REQUEST;
use tokio::sync::Semaphore;
use tokio::time::{sleep, Duration, Instant};
use tracing::{debug, error, info, warn, Instrument};
use zeroize::Zeroizing;

use crate::commands::shared::network::NetworkArgs;
const SEGMENT_HEADERS_LIMIT: u32 = MAX_SEGMENT_HEADERS_PER_REQUEST as u32;

/// Arguments for farmer
#[derive(Debug, Parser)]
pub struct CacheServerArgs {
    cache_path: DiskFarm,
    /// WebSocket RPC URL of the Subspace node to connect to
    #[arg(long, value_hint = ValueHint::Url, default_value = "ws://127.0.0.1:9944")]
    node_rpc_url: String,

    #[arg(long, default_value_t = 10)]
    download_count: u32,

    #[arg(long)]
    http_server: Option<String>,

    #[arg(long, default_value_t = false)]
    pub verify_piece: bool,

    #[arg(long, default_value_t = false)]
    pub disable_detect_future_piece: bool,

    /// DSN parameters
    #[clap(flatten)]
    dsn: NetworkArgs,
}

/// Start farming by using multiple replica plot in specified path and connecting to WebSocket
/// server at specified address.
pub async fn cache_server(cache_server_args: CacheServerArgs) -> anyhow::Result<()> {
    let signal = shutdown_signal();

    let CacheServerArgs {
        node_rpc_url,
        download_count,
        http_server,
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
    info!("use piece directory {:?}", &piece_dir);
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

        configure_network(
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

    let (sender, mut reciever) =
        channel::<(PieceIndex, bool, Option<oneshot::Sender<Option<()>>>)>(50);
    {
        let node = node.clone();
        let node_client = node_client.clone();
        let piece_storage = piece_storage.clone();
        let http_server = http_server.clone();
        //download piece
        tokio::spawn(async move {
            let kzg = Kzg::new();
            let validator = SegmentCommitmentPieceValidator::new(
                node.clone(),
                node_client.clone(),
                kzg.clone(),
            );

            let piece_storage = Arc::new(RwLock::new(piece_storage));
            let semaphore = Arc::new(Semaphore::new(download_count as usize));
            let http_server = http_server.clone();
            info!("Start piece download consumer");
            loop {
                if let Some((piece_index, only_l1, result_sender)) = reciever.next().await {
                    if piece_storage.read().unwrap().has_piece(&piece_index) {
                        continue;
                    }

                    let node = node.clone();
                    let validator = validator.clone();
                    let piece_storage = piece_storage.clone();
                    let http_server = http_server.clone();
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    let _ = tokio::spawn(async move {
                        {
                            if let Some(http_server) = http_server.as_ref() {
                                info!(%piece_index, "Start to download piece from http server {}", http_server);
                                //download from httpserver
                                let segment_key = piece_index.segment_index();
                                let piece_key = piece_index.to_string();

                                let segment_dir = PathBuf::from(segment_key.to_string());
                                let piece_path = segment_dir.join(piece_key.clone());

                                let start = Instant::now();
                                let client = reqwest::Client::new();
                                let getter = client
                                    .get(&(http_server.clone() + piece_path.to_str().unwrap()))
                                    .header("X-Auth-Token", "5b34522870adf849033e33a637395c34")
                                    .send();
                                match getter.await {
                              
                                    Ok(resp) => {
                                        match resp.bytes().await {
                                            Ok(body) => match Piece::try_from(body.to_vec()) {
                                                Ok(piece) => {
                                                    piece_storage
                                                        .write()
                                                        .unwrap()
                                                        .save_piece(piece_index.clone(), piece)
                                                        .expect("Write piece file to storage");
                                                    let duration = start.elapsed();
                                                    drop(permit);
                                                    info!(%piece_index, "Downloaded piece from L1 {:?}", duration);
                                                    if let Some(result_sender) = result_sender {
                                                        if let Err(Some(e)) =
                                                            result_sender.send(Some(()))
                                                        {
                                                            error!(
                                                                "Send download response fail {:?}",
                                                                e
                                                            );
                                                        };
                                                    }
                                                    return;
                                                }
                                                Err(_) => {
                                                    warn!("http server give a wrong piece")
                                                }
                                            },
                                            Err(err) => {
                                                warn!("request piece body fail from http server {err}")
                                            }
                                        }
                                    },
                                    Err(err) => warn!("request piece fail from http server {err}"),
                                }
                            }
                        }
                        info!(%piece_index, "Start to download piece from L2 cache");
                        let piece_provider = PieceProvider::new(node, validator);
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
                            if let Some(result_sender) = result_sender {
                                if let Err(Some(e)) = result_sender.send(Some(())) {
                                    error!("Send download response fail {:?}", e);
                                };
                            }

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
                                if let Some(result_sender) = result_sender {
                                    if let Err(Some(e)) = result_sender.send(Some(())) {
                                        error!("Send download response fail {:?}", e);
                                    };
                                }
                                return;
                            }
                        }

                        error!(%piece_index, "Unable to download piece wait for next round");
                        if let Some(result_sender) = result_sender {
                            if let Err(Some(e)) = result_sender.send(None) {
                                error!("Send download response fail {:?}", e);
                            };
                        }
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
                                        let latest_piece_index = segment_index.last_piece_index();
                                        info!(%segment_index, %latest_piece_index, "Starting to process newly archived segment");
                                        let piecse_indexs = segment_index.segment_piece_indexes();
                                        for piece_index in piecse_indexs {
                                            if let Err(e) = sender.send((piece_index, false, None)).await {
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
                            info!(
                                "Start to download missing pieces {}, latest piece index {}",
                                missing_pieces.len(),
                                last_segment_index.last_piece_index()
                            );
                            for piece_index in missing_pieces {
                                if let Err(e) = sender.send((piece_index, false, None)).await {
                                    warn!(%e, "Send piece index fail");
                                    continue;
                                }
                            }
                        } else {
                            info!(
                                "no missing pieces, latest piece index {}",
                                last_segment_index.last_piece_index()
                            )
                        }

                        if !disable_detect_future_piece {
                            info!("Try to increase piece index");
                            if let Ok(max_piece_index) = piece_storage.max_piece_index() {
                                let mut next_piece_index = max_piece_index + PieceIndex::ONE;
                                loop {
                                    let (result_sender, result_recevier) =
                                        oneshot::channel::<Option<()>>();
                                    if let Err(e) = sender
                                        .send((next_piece_index, true, Some(result_sender)))
                                        .await
                                    {
                                        warn!(%e, "Send piece index fail");
                                        continue;
                                    }

                                    //wait result to stop detect
                                    match result_recevier.await {
                                        Ok(result) => match result {
                                            Some(()) => {
                                                info!(%next_piece_index, "Success get piece more and node maybe sync slow")
                                            }
                                            None => {
                                                info!("no new piece index dectect");
                                                break;
                                            }
                                        },
                                        Err(e) => {
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
                                    if *actual_checksum != *expected_checksum {
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

    fn has_pieces(&self, indexs: Vec<PieceIndex>) -> Vec<PieceIndex> {
        let pieces = self
            .pieces
            .read()
            .map_err(|e| anyhow!(e.to_string()))
            .unwrap();
        let mut find_pieces = vec![];
        for index in indexs {
            if pieces.contains(&index) {
                find_pieces.push(index.clone());
            }
        }
        find_pieces
    }

    fn piece_count(&self) -> Result<usize> {
        let pieces = self.pieces.write().map_err(|e| anyhow!(e.to_string()))?;
        Ok(pieces.len())
    }

    fn max_piece_index(&self) -> Result<PieceIndex> {
        let pieces = self.pieces.read().map_err(|e| anyhow!(e.to_string()))?;
        Ok(*pieces.iter().max().unwrap_or(&PieceIndex::ZERO))
    }

    fn get_piece(&self, piece_index: &PieceIndex) -> Result<Option<Piece>> {
        let _pieces = self.pieces.write().map_err(|e| anyhow!(e.to_string()))?;
        let (_, piece_path, _) = self.piece_path(&piece_index);
        if !piece_path.as_path().is_file() {
            return Err(anyhow!("piece not file or not exit"));
        }
        if !fs::exists(piece_path.clone())? {
            return Ok(None);
        }
        let content = fs::read(piece_path.clone()).expect("Piece file exit");
        Ok(Some(
            Piece::try_from(content).map_err(|err| anyhow!("{:?}", err))?,
        ))
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
        if *actual_checksum != *expected_checksum {
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
        Ok(Piece::try_from(content).map_err(|err| anyhow!("{:?}", err))?)
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
            fs::write(&piece_path, piece)?;
            info!("write piece to {:?}", piece_path);
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

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn configure_network(
    protocol_prefix: String,
    base_path: &Path,
    keypair: Keypair,
    NetworkArgs {
        listen_on,
        bootstrap_nodes,
        allow_private_ips,
        reserved_peers,
        in_connections,
        out_connections,
        pending_in_connections,
        pending_out_connections,
        external_addresses,
    }: NetworkArgs,
    node_client: NodeRetryRpcClient,
    farmer_cache: MyPieceCache,
) -> Result<(Node, NodeRunner<MyPieceCache>), anyhow::Error> {
    let known_peers_registry = KnownPeersManager::new(KnownPeersManagerConfig {
        path: Some(base_path.join("known_addresses.bin").into_boxed_path()),
        ignore_peer_list: strip_peer_id(bootstrap_nodes.clone())
            .into_iter()
            .map(|(peer_id, _)| peer_id)
            .collect::<HashSet<_>>(),
        cache_size: KNOWN_PEERS_CACHE_SIZE,
        ..Default::default()
    })
    .map(Box::new)?;

    let maybe_weak_node = Arc::new(Mutex::new(None::<WeakNode>));
    let default_config = Config::new(protocol_prefix, keypair, farmer_cache.clone(), None);
    let config = Config {
        reserved_peers,
        listen_on,
        allow_non_global_addresses_in_dht: allow_private_ips,
        known_peers_registry,
        request_response_protocols: vec![
            {
                let maybe_weak_node = Arc::clone(&maybe_weak_node);
                let farmer_cache = farmer_cache.clone();

                CachedPieceByIndexRequestHandler::create(move |peer_id, request| {
                    //todo disable
                    let CachedPieceByIndexRequest {
                        piece_index,
                        cached_pieces,
                    } = request;
                    debug!(?piece_index, "Cached piece request received");

                    let maybe_weak_node = Arc::clone(&maybe_weak_node);
                    let farmer_cache = farmer_cache.clone();
                    let mut cached_pieces = Arc::unwrap_or_clone(cached_pieces);

                    async move {
                        let piece_from_cache = farmer_cache.get_piece(&piece_index);
                        cached_pieces.truncate(CachedPieceByIndexRequest::RECOMMENDED_LIMIT);
                        let cached_pieces = farmer_cache.has_pieces(cached_pieces);

                        Some(CachedPieceByIndexResponse {
                            result: match piece_from_cache {
                                Ok(Some(piece)) => PieceResult::Piece(piece),
                                Ok(None) => {
                                    let maybe_node = maybe_weak_node
                                        .lock()
                                        .as_ref()
                                        .expect("Always called after network instantiation; qed")
                                        .upgrade();

                                    let closest_peers = if let Some(node) = maybe_node {
                                        node.get_closest_local_peers(
                                            piece_index.to_multihash(),
                                            Some(peer_id),
                                        )
                                        .await
                                        .inspect_err(|error| {
                                            warn!(%error, "Failed to get closest local peers");
                                        })
                                        .unwrap_or_default()
                                    } else {
                                        Vec::new()
                                    };

                                    PieceResult::ClosestPeers(closest_peers.into())
                                }
                                Err(_) => PieceResult::ClosestPeers(Vec::new().into()),
                            },
                            cached_pieces,
                        })
                    }
                    .in_current_span()
                })
            },
            PieceByIndexRequestHandler::create(move |_, request| {
                //todo disable
                let PieceByIndexRequest {
                    piece_index,
                    cached_pieces,
                } = request;
                debug!(?piece_index, "Piece request received. Trying cache...");

                let farmer_cache = farmer_cache.clone();
                let mut cached_pieces = Arc::unwrap_or_clone(cached_pieces);

                async move {
                    let piece_from_cache = farmer_cache.get_piece(&piece_index);
                    cached_pieces.truncate(PieceByIndexRequest::RECOMMENDED_LIMIT);
                    let cached_pieces = farmer_cache.has_pieces(cached_pieces);

                    if let Ok(Some(piece)) = piece_from_cache {
                        Some(PieceByIndexResponse {
                            piece: Some(piece),
                            cached_pieces,
                        })
                    } else {
                        debug!(
                            ?piece_index,
                            "No piece in the cache. Trying archival storage..."
                        );

                        return None;
                    }
                }
                .in_current_span()
            }),
            SegmentHeaderBySegmentIndexesRequestHandler::create(move |_, req| {
                //todo disable
                info!(?req, "Segment headers request received.");

                let node_client = node_client.clone();

                async move {
                    let internal_result = match req {
                        SegmentHeaderRequest::SegmentIndexes { segment_indexes } => {
                            let segment_indexes = Arc::unwrap_or_clone(segment_indexes);

                            if segment_indexes.len() > SEGMENT_HEADERS_LIMIT as usize {
                                debug!(
                                    "segment_indexes length exceed the limit: {} ",
                                    segment_indexes.len()
                                );

                                return None;
                            }

                            debug!(
                                segment_indexes_count = ?segment_indexes.len(),
                                "Segment headers request received."
                            );

                            node_client.segment_headers(segment_indexes).await
                        }
                        SegmentHeaderRequest::LastSegmentHeaders { mut limit } => {
                            if limit > SEGMENT_HEADERS_LIMIT {
                                debug!(
                                    %limit,
                                    "Segment header number exceeded the limit."
                                );

                                limit = SEGMENT_HEADERS_LIMIT;
                            }
                            node_client.last_segment_headers(limit).await
                        }
                    };

                    match internal_result {
                        Ok(segment_headers) => segment_headers
                            .into_iter()
                            .inspect(|maybe_segment_header| {
                                if maybe_segment_header.is_none() {
                                    error!("Received empty optional segment header!");
                                }
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
        ..default_config
    };

    let (node, node_runner) = construct(config)?;
    maybe_weak_node.lock().replace(node.downgrade());

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
    Ok((node, node_runner))
}
