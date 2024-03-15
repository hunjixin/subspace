use clap::Parser;
use sc_cli::Error;
use sc_consensus_subspace_rpc::{NodeRpcClient, NodeClient};

/// Options for creating domain key
#[derive(Debug, Parser)]
pub struct ListConnectPeersOptions {
    /// RpcURL .
    #[clap(short, long, default_value = "http://127.0.0.1:9944")]
    pub rpc_url: String,
}

pub fn list_connected_peers(options: ListConnectPeersOptions) -> Result<(), Error> {
    let ListConnectPeersOptions {
        rpc_url,
    } = options;
    let node_client = NodeRpcClient::new(&rpc_url).await?;
    node_client.

    Ok(())
}