use std::pin::Pin;

use crate::util::{
    page::PageType, table_slot::TableSlot, tape::Tape, HeapPointer, IndexPointer, ItemPointer,
};

use super::{
    graph::{ListSearchNeighbor, ListSearchResult},
    graph_neighbor_store::GraphNeighborStore,
    meta_page::MetaPage,
    neighbor_with_distance::NeighborWithDistance,
    pg_vector::PgVector,
    stats::{
        GreedySearchStats, StatsDistanceComparison, StatsNodeModify, StatsNodeRead, StatsNodeWrite,
        WriteStats,
    },
};

/// NodeDistanceMeasure keeps the state to make distance comparison between two nodes.
pub trait NodeDistanceMeasure {
    unsafe fn get_distance<S: StatsNodeRead + StatsDistanceComparison>(
        &self,
        index_pointer: IndexPointer,
        stats: &mut S,
    ) -> f32;
}

pub trait ArchivedData {
    fn with_data(data: &mut [u8]) -> Pin<&mut Self>;
    fn is_deleted(&self) -> bool;
    fn delete(self: Pin<&mut Self>);
    fn get_heap_item_pointer(&self) -> HeapPointer;
    fn get_index_pointer_to_neighbors(&self) -> Vec<ItemPointer>;
}

pub trait Storage {
    /// A QueryDistanceMeasure keeps the state to make distance comparison between a query given at initialization and a node.
    type QueryDistanceMeasure;
    /// A NodeDistanceMeasure keeps the state to make distance comparison between a node given at initialization and another node.
    type NodeDistanceMeasure<'a>: NodeDistanceMeasure
    where
        Self: 'a;
    type ArchivedType: ArchivedData;
    type LSNPrivateData;

    fn page_type() -> PageType;

    fn create_node<S: StatsNodeWrite>(
        &self,
        full_vector: &[f32],
        heap_pointer: HeapPointer,
        meta_page: &MetaPage,
        tape: &mut Tape,
        stats: &mut S,
    ) -> ItemPointer;

    fn start_training(&mut self, meta_page: &super::meta_page::MetaPage);
    fn add_sample(&mut self, sample: &[f32]);
    fn finish_training(&mut self, stats: &mut WriteStats);

    fn finalize_node_at_end_of_build<S: StatsNodeRead + StatsNodeModify>(
        &mut self,
        meta: &MetaPage,
        index_pointer: IndexPointer,
        neighbors: &Vec<NeighborWithDistance>,
        stats: &mut S,
    );

    unsafe fn get_node_distance_measure<'a, S: StatsNodeRead>(
        &'a self,
        index_pointer: IndexPointer,
        stats: &mut S,
    ) -> Self::NodeDistanceMeasure<'a>;

    fn get_query_distance_measure(&self, query: PgVector) -> Self::QueryDistanceMeasure;

    fn visit_lsn(
        &self,
        lsr: &mut ListSearchResult<Self::QueryDistanceMeasure, Self::LSNPrivateData>,
        lsn_idx: usize,
        gns: &GraphNeighborStore,
    ) where
        Self: Sized;

    fn create_lsn_for_init_id(
        &self,
        lsr: &mut ListSearchResult<Self::QueryDistanceMeasure, Self::LSNPrivateData>,
        index_pointer: ItemPointer,
        gns: &GraphNeighborStore,
    ) -> ListSearchNeighbor<Self::LSNPrivateData>
    where
        Self: Sized;

    fn return_lsn(
        &self,
        lsn: &ListSearchNeighbor<Self::LSNPrivateData>,
        stats: &mut GreedySearchStats,
    ) -> HeapPointer
    where
        Self: Sized;

    fn get_neighbors_with_distances_from_disk<S: StatsNodeRead + StatsDistanceComparison>(
        &self,
        neighbors_of: ItemPointer,
        result: &mut Vec<NeighborWithDistance>,
        stats: &mut S,
    );

    fn set_neighbors_on_disk<S: StatsNodeModify + StatsNodeRead>(
        &self,
        meta: &MetaPage,
        index_pointer: IndexPointer,
        neighbors: &[NeighborWithDistance],
        stats: &mut S,
    );

    fn get_distance_function(&self) -> fn(&[f32], &[f32]) -> f32;
}

pub trait StorageFullDistanceFromHeap {
    unsafe fn get_heap_table_slot_from_index_pointer<T: StatsNodeRead>(
        &self,
        index_pointer: IndexPointer,
        stats: &mut T,
    ) -> TableSlot;

    unsafe fn get_heap_table_slot_from_heap_pointer<T: StatsNodeRead>(
        &self,
        heap_pointer: HeapPointer,
        stats: &mut T,
    ) -> TableSlot;
}

pub enum StorageType {
    BqSpeedup,
    PqCompression,
    Plain,
}
