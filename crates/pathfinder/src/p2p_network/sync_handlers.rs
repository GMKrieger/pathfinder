use anyhow::Context;
use futures::channel::mpsc;
use futures::SinkExt;
use p2p_proto::block::{
    BlockBodiesRequest, BlockBodiesResponse, BlockBodyMessage, BlockHeadersRequest,
    BlockHeadersResponse, BlockHeadersResponsePart, Signatures,
};
use p2p_proto::common::{
    BlockId, BlockNumberOrHash, ConsensusSignature, Direction, Fin, Hash, Iteration, Step,
};
use p2p_proto::consts::MAX_HEADERS_PER_MESSAGE;
use p2p_proto::event::{Events, EventsRequest, EventsResponse, EventsResponseKind};
use p2p_proto::receipt::{Receipts, ReceiptsRequest, ReceiptsResponse, ReceiptsResponseKind};
use p2p_proto::state::{Class, Classes};
use p2p_proto::transaction::{
    Transactions, TransactionsRequest, TransactionsResponse, TransactionsResponseKind,
};
use pathfinder_common::{BlockHash, BlockNumber, CasmHash, ClassHash, SierraHash};
use pathfinder_storage::Storage;
use pathfinder_storage::Transaction;

pub mod conv;
#[cfg(test)]
mod tests;

use conv::ToProto;

#[cfg(not(test))]
const MAX_BLOCKS_COUNT: u64 = 100;

#[cfg(test)]
const MAX_COUNT_IN_TESTS: u64 = 10;
#[cfg(test)]
const MAX_BLOCKS_COUNT: u64 = MAX_COUNT_IN_TESTS;

const _: () = assert!(
    MAX_BLOCKS_COUNT <= MAX_HEADERS_PER_MESSAGE as u64,
    "All requested block headers, limited up to MAX_BLOCKS_COUNT should fit into one reply"
);

pub async fn get_headers(
    storage: Storage,
    request: BlockHeadersRequest,
    mut tx: mpsc::Sender<BlockHeadersResponse>,
) -> anyhow::Result<()> {
    let response = spawn_blocking_get(request, storage, blocking::get_headers).await?;
    tx.send(response).await.context("Sending response")
}

// TODO consider batching db ops instead doing all in bulk if it's more performant
pub async fn get_bodies(
    storage: Storage,
    request: BlockBodiesRequest,
    tx: mpsc::Sender<BlockBodiesResponse>,
) -> anyhow::Result<()> {
    let responses = spawn_blocking_get(request, storage, blocking::get_bodies).await?;
    send(tx, responses).await
}

pub async fn get_transactions(
    storage: Storage,
    request: TransactionsRequest,
    tx: mpsc::Sender<TransactionsResponse>,
) -> anyhow::Result<()> {
    let responses = spawn_blocking_get(request, storage, blocking::get_transactions).await?;
    send(tx, responses).await
}

pub async fn get_receipts(
    storage: Storage,
    request: ReceiptsRequest,
    tx: mpsc::Sender<ReceiptsResponse>,
) -> anyhow::Result<()> {
    let responses = spawn_blocking_get(request, storage, blocking::get_receipts).await?;
    send(tx, responses).await
}

pub async fn get_events(
    storage: Storage,
    request: EventsRequest,
    tx: mpsc::Sender<EventsResponse>,
) -> anyhow::Result<()> {
    let responses = spawn_blocking_get(request, storage, blocking::get_events).await?;
    send(tx, responses).await
}

pub(crate) mod blocking {
    use super::*;

    pub(crate) fn get_headers(
        tx: Transaction<'_>,
        request: BlockHeadersRequest,
    ) -> anyhow::Result<BlockHeadersResponse> {
        let parts = iterate(tx, request.iteration, get_header)?;
        Ok(BlockHeadersResponse { parts })
    }

    pub(crate) fn get_bodies(
        tx: Transaction<'_>,
        request: BlockBodiesRequest,
    ) -> anyhow::Result<Vec<BlockBodiesResponse>> {
        iterate(tx, request.iteration, get_body)
    }

    pub(crate) fn get_transactions(
        tx: Transaction<'_>,
        request: TransactionsRequest,
    ) -> anyhow::Result<Vec<TransactionsResponse>> {
        iterate(tx, request.iteration, get_transactions_for_block)
    }

    pub(crate) fn get_receipts(
        tx: Transaction<'_>,
        request: ReceiptsRequest,
    ) -> anyhow::Result<Vec<ReceiptsResponse>> {
        iterate(tx, request.iteration, get_receipts_for_block)
    }

    pub(crate) fn get_events(
        tx: Transaction<'_>,
        request: EventsRequest,
    ) -> anyhow::Result<Vec<EventsResponse>> {
        iterate(tx, request.iteration, get_events_for_block)
    }
}

fn get_header(
    tx: &Transaction<'_>,
    block_number: BlockNumber,
    parts: &mut Vec<BlockHeadersResponsePart>,
) -> anyhow::Result<bool> {
    if let Some(header) = tx.block_header(block_number.into())? {
        let hash = Hash(header.hash.0);
        parts.push(BlockHeadersResponsePart::Header(Box::new(
            header.to_proto(),
        )));

        if let Some(signature) = tx.signature(block_number.into())? {
            parts.push(BlockHeadersResponsePart::Signatures(Signatures {
                block: BlockId {
                    number: block_number.get(),
                    hash,
                },
                signatures: vec![ConsensusSignature {
                    r: signature.r.0,
                    s: signature.s.0,
                }],
            }));
        }

        parts.push(BlockHeadersResponsePart::Fin(Fin::ok()));

        Ok(true)
    } else {
        Ok(false)
    }
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(fake::Dummy))]
enum ClassId {
    Cairo(ClassHash),
    Sierra(SierraHash, CasmHash),
}

impl ClassId {
    pub fn class_hash(&self) -> ClassHash {
        match self {
            ClassId::Cairo(class_hash) => *class_hash,
            ClassId::Sierra(sierra_hash, _) => ClassHash(sierra_hash.0),
        }
    }
}

#[derive(Debug, Clone)]
enum ClassDefinition {
    Cairo(Vec<u8>),
    Sierra { sierra: Vec<u8>, casm: Vec<u8> },
}

fn get_body(
    tx: &Transaction<'_>,
    block_number: BlockNumber,
    responses: &mut Vec<BlockBodiesResponse>,
) -> anyhow::Result<bool> {
    let Some(state_diff) = tx.state_update(block_number.into())? else {
        return Ok(false);
    };

    let new_classes = state_diff
        .declared_cairo_classes
        .iter()
        .map(|&class_hash| ClassId::Cairo(class_hash))
        .chain(
            state_diff
                .declared_sierra_classes
                .iter()
                .map(|(&sierra_hash, &casm_hash)| ClassId::Sierra(sierra_hash, casm_hash)),
        )
        .collect::<Vec<_>>();
    let block_hash = state_diff.block_hash;
    let id = Some(BlockId {
        number: block_number.get(),
        hash: Hash(block_hash.0),
    });

    responses.push(BlockBodiesResponse {
        id,
        body_message: BlockBodyMessage::Diff(state_diff.to_proto()),
    });

    let get_definition =
        |block_number: BlockNumber, class_hash| -> anyhow::Result<ClassDefinition> {
            let definition = tx
                .class_definition_at(block_number.into(), class_hash)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Class definition {} not found at block {}",
                        class_hash,
                        block_number
                    )
                })?;
            let casm_definition = tx.casm_definition(class_hash)?;
            Ok(match casm_definition {
                Some(casm) => ClassDefinition::Sierra {
                    sierra: definition,
                    casm,
                },
                None => ClassDefinition::Cairo(definition),
            })
        };

    classes(
        block_number,
        block_hash,
        new_classes,
        responses,
        get_definition,
    )?;

    responses.push(BlockBodiesResponse {
        id,
        body_message: BlockBodyMessage::Fin(Fin::ok()),
    });
    Ok(true)
}

fn get_transactions_for_block(
    tx: &Transaction<'_>,
    block_number: BlockNumber,
    responses: &mut Vec<TransactionsResponse>,
) -> anyhow::Result<bool> {
    let Some((_, block_hash)) = tx.block_id(block_number.into())? else {
        return Ok(false);
    };

    let Some(txn_data) = tx.transaction_data_for_block(block_number.into())? else {
        return Ok(false);
    };

    let id = Some(BlockId {
        number: block_number.get(),
        hash: Hash(block_hash.0),
    });

    responses.push(TransactionsResponse {
        id,
        kind: TransactionsResponseKind::Transactions(Transactions {
            items: txn_data
                .into_iter()
                .map(|(txn, _)| pathfinder_common::transaction::Transaction::from(txn).to_proto())
                .collect(),
        }),
    });
    responses.push(TransactionsResponse {
        id,
        kind: TransactionsResponseKind::Fin(Fin::ok()),
    });

    Ok(true)
}

fn get_receipts_for_block(
    tx: &Transaction<'_>,
    block_number: BlockNumber,
    responses: &mut Vec<ReceiptsResponse>,
) -> anyhow::Result<bool> {
    let Some((_, block_hash)) = tx.block_id(block_number.into())? else {
        return Ok(false);
    };

    let Some(txn_data) = tx.transaction_data_for_block(block_number.into())? else {
        return Ok(false);
    };

    let id = Some(BlockId {
        number: block_number.get(),
        hash: Hash(block_hash.0),
    });

    responses.push(ReceiptsResponse {
        id,
        kind: ReceiptsResponseKind::Receipts(Receipts {
            items: txn_data.into_iter().map(ToProto::to_proto).collect(),
        }),
    });
    responses.push(ReceiptsResponse {
        id,
        kind: ReceiptsResponseKind::Fin(Fin::ok()),
    });

    Ok(true)
}

fn get_events_for_block(
    tx: &Transaction<'_>,
    block_number: BlockNumber,
    responses: &mut Vec<EventsResponse>,
) -> anyhow::Result<bool> {
    let Some((_, block_hash)) = tx.block_id(block_number.into())? else {
        return Ok(false);
    };

    let Some(txn_data) = tx.transaction_data_for_block(block_number.into())? else {
        return Ok(false);
    };

    let items = txn_data
        .into_iter()
        .flat_map(|(_, r)| {
            std::iter::repeat(r.transaction_hash)
                .zip(r.events)
                .map(ToProto::to_proto)
        })
        .collect::<Vec<_>>();

    let id = Some(BlockId {
        number: block_number.get(),
        hash: Hash(block_hash.0),
    });

    responses.push(EventsResponse {
        id,
        kind: EventsResponseKind::Events(Events { items }),
    });
    responses.push(EventsResponse {
        id,
        kind: EventsResponseKind::Fin(Fin::ok()),
    });

    Ok(true)
}

/// `block_handler` returns Ok(true) if the iteration should continue and is
/// responsible for delimiting block data with `Fin::ok()` marker.
fn iterate<T: From<Fin>>(
    tx: Transaction<'_>,
    iteration: Iteration,
    block_handler: impl Fn(&Transaction<'_>, BlockNumber, &mut Vec<T>) -> anyhow::Result<bool>,
) -> anyhow::Result<Vec<T>> {
    let Iteration {
        start,
        direction,
        limit,
        step,
    } = iteration;

    if limit == 0 {
        return Ok(vec![T::from(Fin::ok())]);
    }

    let mut block_number = match get_start_block_number(start, &tx)? {
        Some(x) => x,
        None => {
            return Ok(vec![T::from(Fin::unknown())]);
        }
    };

    let (limit, mut ending_marker) = if limit > MAX_BLOCKS_COUNT {
        (MAX_BLOCKS_COUNT, Some(Fin::too_much()))
    } else {
        (limit, None)
    };

    let mut responses = Vec::new();

    for i in 0..limit {
        if block_handler(&tx, block_number, &mut responses)? {
            // Block data retrieved successfully, `block_handler` should add `Fin::ok()` marker on its own
        } else {
            // No such block
            ending_marker = Some(Fin::unknown());
            break;
        }

        if i < limit - 1 {
            block_number = match get_next_block_number(block_number, step, direction) {
                Some(x) => x,
                None => {
                    // Out of range block number value
                    ending_marker = Some(Fin::unknown());
                    break;
                }
            };
        }
    }

    if let Some(end) = ending_marker {
        responses.push(T::from(end));
    }

    Ok(responses)
}

fn get_start_block_number(
    start: BlockNumberOrHash,
    tx: &Transaction<'_>,
) -> Result<Option<BlockNumber>, anyhow::Error> {
    Ok(match start {
        BlockNumberOrHash::Number(n) => BlockNumber::new(n),
        BlockNumberOrHash::Hash(h) => tx.block_id(BlockHash(h.0).into())?.map(|(n, _)| n),
    })
}

fn classes(
    block_number: BlockNumber,
    block_hash: BlockHash,
    new_class_ids: Vec<ClassId>,
    responses: &mut Vec<BlockBodiesResponse>,
    mut class_definition_getter: impl FnMut(BlockNumber, ClassHash) -> anyhow::Result<ClassDefinition>,
) -> anyhow::Result<()> {
    let mut classes = Vec::new();

    for class_id in new_class_ids {
        let class_definition = class_definition_getter(block_number, class_id.class_hash())?;

        let class: Class = match (class_id, class_definition) {
            (ClassId::Cairo(_), ClassDefinition::Cairo(definition)) => {
                let cairo_class =
                    serde_json::from_slice::<class_definition::de::Cairo<'_>>(&definition)?;
                Class::Cairo0(cairo_class.into_dto())
            }
            (ClassId::Sierra(_, _), ClassDefinition::Sierra { sierra, casm }) => {
                let sierra_class =
                    serde_json::from_slice::<class_definition::de::Sierra<'_>>(&sierra)?;
                Class::Cairo1(sierra_class.into_dto(casm))
            }
            _ => anyhow::bail!("Class definition type mismatch"),
        };
        classes.push(class);
    }

    responses.push(BlockBodiesResponse {
        id: Some(BlockId {
            number: block_number.get(),
            hash: Hash(block_hash.0),
        }),
        body_message: BlockBodyMessage::Classes(Classes {
            domain: 0, // TODO
            classes,
        }),
    });

    Ok(())
}

async fn spawn_blocking_get<Request, Response, Getter>(
    request: Request,
    storage: Storage,
    getter: Getter,
) -> anyhow::Result<Response>
where
    Request: Send + 'static,
    Response: Send + 'static,
    Getter: FnOnce(Transaction<'_>, Request) -> anyhow::Result<Response> + Send + 'static,
{
    let span = tracing::Span::current();

    tokio::task::spawn_blocking(move || {
        let _g = span.enter();
        let mut connection = storage
            .connection()
            .context("Opening database connection")?;
        let tx = connection
            .transaction()
            .context("Creating database transaction")?;
        getter(tx, request)
    })
    .await
    .context("Database read panic or shutting down")?
}

async fn send<T>(mut tx: mpsc::Sender<T>, seq: Vec<T>) -> anyhow::Result<()>
where
    T: Send + 'static,
    tokio::sync::mpsc::error::SendError<T>: Sync,
{
    for elem in seq {
        tx.send(elem).await.context("Sending response")?;
    }

    Ok(())
}

/// Returns next block number considering direction.
///
/// None is returned if we're out-of-bounds.
fn get_next_block_number(
    current: BlockNumber,
    step: Step,
    direction: Direction,
) -> Option<BlockNumber> {
    match direction {
        Direction::Forward => current
            .get()
            .checked_add(step.into_inner())
            .and_then(BlockNumber::new),
        Direction::Backward => current
            .get()
            .checked_sub(step.into_inner())
            .and_then(BlockNumber::new),
    }
}

mod class_definition {
    pub mod de {
        use super::common::{CairoEntryPoints, SierraEntryPoints};
        use p2p_proto::state::{Cairo0Class, Cairo1Class, EntryPoint};
        use pathfinder_crypto::Felt;
        use serde::Deserialize;
        use starknet_gateway_types::request::contract::SelectorAndOffset;

        /// A version of [`starknet_gateway_types::class_hash::json::SierraContractDefinition`]
        /// used to deserialize from storage.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Sierra<'a> {
            /// Contract ABI.
            #[serde(borrow)]
            pub abi: &'a str,

            /// Main program definition.
            pub sierra_program: Vec<Felt>,

            // Version
            #[serde(borrow)]
            pub contract_class_version: &'a str,

            /// The contract entry points
            pub entry_points_by_type: SierraEntryPoints,
        }

        impl<'a> Sierra<'a> {
            pub fn into_dto(self, compiled: Vec<u8>) -> Cairo1Class {
                Cairo1Class {
                    abi: self.abi.as_bytes().to_owned(),
                    program: self.sierra_program,
                    entry_points: self.entry_points_by_type.into_dto(),
                    compiled, // TODO not sure if encoding in storage and dto is the same
                    contract_class_version: self.contract_class_version.to_owned(),
                }
            }
        }

        /// A "shallow" version of the [`starknet_gateway_types::class_hash::json::CairoContractDefinition`]
        /// used to deserialize from storage. Assumes that `program` is valid JSON.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Cairo<'a> {
            /// Contract ABI, which has no schema definition.
            #[serde(borrow)]
            pub abi: &'a str,

            /// Main program definition. __We assume that this is valid JSON.__
            #[serde(borrow)]
            pub program: &'a serde_json::value::RawValue,

            /// The contract entry points.
            pub entry_points_by_type: CairoEntryPoints,
        }

        impl<'a> Cairo<'a> {
            pub fn into_dto(self) -> Cairo0Class {
                let into_cairo = |x: SelectorAndOffset| EntryPoint {
                    selector: x.selector.0,
                    offset: x.offset.0,
                };

                Cairo0Class {
                    abi: self.abi.as_bytes().to_owned(),
                    externals: self
                        .entry_points_by_type
                        .external
                        .into_iter()
                        .map(into_cairo)
                        .collect(),
                    l1_handlers: self
                        .entry_points_by_type
                        .l1_handler
                        .into_iter()
                        .map(into_cairo)
                        .collect(),
                    constructors: self
                        .entry_points_by_type
                        .constructor
                        .into_iter()
                        .map(into_cairo)
                        .collect(),
                    program: self.program.get().as_bytes().to_owned(),
                }
            }
        }
    }

    /// Used for generating fake data.
    pub mod fake {
        use super::common::{CairoEntryPoints, SierraEntryPoints};
        use fake::{Dummy, Fake, Faker};
        use pathfinder_crypto::Felt;
        use rand::Rng;
        use serde::Serialize;

        #[derive(Debug, Dummy, Serialize)]
        pub struct Sierra {
            /// Contract ABI.
            pub abi: String,

            /// Main program definition.
            pub sierra_program: Vec<Felt>,

            // Version
            pub contract_class_version: String,

            /// The contract entry points
            pub entry_points_by_type: SierraEntryPoints,
        }

        #[derive(Debug, Serialize)]
        pub struct Cairo {
            /// Contract ABI, which has no schema definition.
            pub abi: String,

            /// Main program definition. __We assume that this is valid JSON.__
            pub program: serde_json::Value,

            /// The contract entry points.
            pub entry_points_by_type: CairoEntryPoints,
        }

        impl<T> Dummy<T> for Cairo {
            fn dummy_with_rng<R: Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
                let program = serde_json::Value::Object(Faker.fake_with_rng(rng));
                Self {
                    abi: Faker.fake_with_rng(rng),
                    program,
                    entry_points_by_type: Faker.fake_with_rng(rng),
                }
            }
        }
    }

    pub mod common {
        use fake::Dummy;
        use p2p_proto::state::{Cairo1EntryPoints, SierraEntryPoint};
        use serde::{Deserialize, Serialize};
        use starknet_gateway_types::request::contract::{
            SelectorAndFunctionIndex, SelectorAndOffset,
        };

        #[derive(Debug, Deserialize, Serialize, Dummy)]
        #[serde(deny_unknown_fields)]
        pub struct SierraEntryPoints {
            #[serde(rename = "EXTERNAL")]
            pub external: Vec<SelectorAndFunctionIndex>,
            #[serde(rename = "L1_HANDLER")]
            pub l1_handler: Vec<SelectorAndFunctionIndex>,
            #[serde(rename = "CONSTRUCTOR")]
            pub constructor: Vec<SelectorAndFunctionIndex>,
        }

        impl SierraEntryPoints {
            pub fn into_dto(self) -> Cairo1EntryPoints {
                let into_sierra = |x: SelectorAndFunctionIndex| SierraEntryPoint {
                    selector: x.selector.0,
                    index: x.function_idx,
                };

                Cairo1EntryPoints {
                    externals: self.external.into_iter().map(into_sierra).collect(),
                    l1_handlers: self.l1_handler.into_iter().map(into_sierra).collect(),
                    constructors: self.constructor.into_iter().map(into_sierra).collect(),
                }
            }
        }

        #[derive(Debug, Deserialize, Serialize, Dummy)]
        #[serde(deny_unknown_fields)]
        pub struct CairoEntryPoints {
            #[serde(rename = "EXTERNAL")]
            pub external: Vec<SelectorAndOffset>,
            #[serde(rename = "L1_HANDLER")]
            pub l1_handler: Vec<SelectorAndOffset>,
            #[serde(rename = "CONSTRUCTOR")]
            pub constructor: Vec<SelectorAndOffset>,
        }
    }
}
