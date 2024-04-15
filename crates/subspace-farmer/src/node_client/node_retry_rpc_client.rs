use crate::node_client::{Error as RpcError, Error, NodeClient, NodeClientExt};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use jsonrpsee::core::Error as JsonError;
use reconnecting_jsonrpsee_ws_client::{rpc_params, Client, ExponentialBackoff, PingConfig};
use std::fmt::{self};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use subspace_core_primitives::{Piece, PieceIndex, SegmentHeader, SegmentIndex};
use subspace_rpc_primitives::{
    FarmerAppInfo, RewardSignatureResponse, RewardSigningInfo, SlotInfo, SolutionResponse,
};

/// `WsClient` wrapper.
#[derive(Clone)]
pub struct NodeRetryRpcClient {
    client: Arc<Client>,
}
impl NodeRetryRpcClient {
    /// Create a new instance of [`NodeClient`].
    pub async fn new(url: &str) -> Result<Self, JsonError> {
        let client = Arc::new(
            Client::builder()
                .retry_policy(ExponentialBackoff::from_millis(100))
                .disable_ws_ping()
                .build(url.to_string())
                .await
                .map_err(|_| jsonrpsee::core::Error::Custom("init fail".to_string()))?,
        );
        Ok(Self { client })
    }
}

impl fmt::Debug for NodeRetryRpcClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mock")
    }
}

#[async_trait]
impl NodeClient for NodeRetryRpcClient {
    async fn farmer_app_info(&self) -> Result<FarmerAppInfo, Error> {
        let raw = self
            .client
            .request("subspace_getFarmerAppInfo".to_string(), rpc_params![])
            .await?;
        Ok(serde_json::from_str::<FarmerAppInfo>(raw.get())?)
    }

    async fn subscribe_slot_info(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = SlotInfo> + Send + 'static>>, RpcError> {
        let subscription = self
            .client
            .subscribe(
                "subspace_subscribeSlotInfo".to_string(),
                rpc_params![],
                "subspace_unsubscribeSlotInfo".to_string(),
            )
            .await?;

        Ok(Box::pin(subscription.filter_map(
            |slot_info_result| async move {
                match slot_info_result {
                    Ok(raw) => match serde_json::from_str::<SlotInfo>(raw.get()) {
                        Ok(v) => Some(v),
                        Err(_) => None,
                    },
                    Err(_) => None,
                }
            },
        )))
    }

    async fn submit_solution_response(
        &self,
        solution_response: SolutionResponse,
    ) -> Result<(), RpcError> {
        Ok(self
            .client
            .request(
                "subspace_submitSolutionResponse".to_string(),
                rpc_params![&solution_response],
            )
            .await
            .map(|_| ())?)
    }

    async fn subscribe_reward_signing(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = RewardSigningInfo> + Send + 'static>>, RpcError> {
        let subscription = self
            .client
            .subscribe(
                "subspace_subscribeRewardSigning".to_string(),
                rpc_params![],
                "subspace_unsubscribeRewardSigning".to_string(),
            )
            .await?;

        Ok(Box::pin(subscription.filter_map(
            |reward_signing_info_result| async move {
                match reward_signing_info_result {
                    Ok(raw) => match serde_json::from_str::<RewardSigningInfo>(raw.get()) {
                        Ok(v) => Some(v),
                        Err(_) => None,
                    },
                    Err(_) => None,
                }
            },
        )))
    }

    /// Submit a block signature
    async fn submit_reward_signature(
        &self,
        reward_signature: RewardSignatureResponse,
    ) -> Result<(), RpcError> {
        Ok(self
            .client
            .request(
                "subspace_submitRewardSignature".to_string(),
                rpc_params![&reward_signature],
            )
            .await
            .map(|_| ())?)
    }

    async fn subscribe_archived_segment_headers(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = SegmentHeader> + Send + 'static>>, RpcError> {
        let subscription = self
            .client
            .subscribe(
                "subspace_subscribeArchivedSegmentHeader".to_string(),
                rpc_params![],
                "subspace_unsubscribeArchivedSegmentHeader".to_string(),
            )
            .await?;

        Ok(Box::pin(subscription.filter_map(
            |archived_segment_header_result| async move {
                match archived_segment_header_result {
                    Ok(raw) => match serde_json::from_str::<SegmentHeader>(raw.get()) {
                        Ok(v) => Some(v),
                        Err(_) => None,
                    },
                    Err(_) => None,
                }
            },
        )))
    }

    async fn segment_headers(
        &self,
        segment_indexes: Vec<SegmentIndex>,
    ) -> Result<Vec<Option<SegmentHeader>>, RpcError> {
        let raw = self
            .client
            .request(
                "subspace_segmentHeaders".to_string(),
                rpc_params![&segment_indexes],
            )
            .await?;
        Ok(serde_json::from_str::<Vec<Option<SegmentHeader>>>(
            raw.get(),
        )?)
    }

    async fn piece(&self, piece_index: PieceIndex) -> Result<Option<Piece>, RpcError> {
        let raw = self
            .client
            .request("subspace_piece".to_string(), rpc_params![&piece_index])
            .await?;

        let result = serde_json::from_str::<Option<Vec<u8>>>(raw.get())?;
        if let Some(bytes) = result {
            let piece = Piece::try_from(bytes.as_slice())
                .map_err(|_| format!("Cannot convert piece. PieceIndex={}", piece_index))?;

            return Ok(Some(piece));
        }

        Ok(None)
    }

    async fn acknowledge_archived_segment_header(
        &self,
        segment_index: SegmentIndex,
    ) -> Result<(), Error> {
        Ok(
            self
            .client
            .request(
                "subspace_acknowledgeArchivedSegmentHeader".to_string(),
                rpc_params![&segment_index],
            )
            .await
            .map(|_| ())?
        )
    }
}

#[async_trait]
impl NodeClientExt for NodeRetryRpcClient {
    async fn last_segment_headers(
        &self,
        limit: u64,
    ) -> Result<Vec<Option<SegmentHeader>>, RpcError> {
        let raw = self
            .client
            .request(
                "subspace_lastSegmentHeaders".to_string(),
                rpc_params![limit],
            )
            .await?;
        Ok(serde_json::from_str::<Vec<Option<SegmentHeader>>>(
            raw.get(),
        )?)
    }
}
